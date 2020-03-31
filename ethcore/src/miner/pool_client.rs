// Copyright 2018-2020 Origo Foundation.
// This file is part of Origo Network.

// Origo Network is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Origo Network is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Origo Network.  If not, see <http://www.gnu.org/licenses/>.

//! Blockchain access for transaction pool.

use std::{collections::HashMap, fmt, sync::Arc};

use ethcore_miner::local_accounts::LocalAccounts;
use ethcore_miner::pool;
use ethcore_miner::pool::client::{NonceClient, NullifierClient};
use ethcore_miner::service_transaction_checker::ServiceTransactionChecker;
use ethereum_types::{Address, H256, U256};
use parking_lot::RwLock;
use types::header::Header;
use types::transaction::{self, SignedTransaction, UnverifiedTransaction};

use call_contract::CallContract;
use client::{BlockInfo, Nonce, TransactionId, TransactionInfo};
use engines::EthEngine;
use miner;
use transaction_ext::Transaction;
use zcash_primitives::sapling::Node;
use ff::PrimeField;

/// Cache for state nonces.
#[derive(Debug, Clone)]
pub struct NonceCache {
	nonces: Arc<RwLock<HashMap<Address, U256>>>,
	limit: usize,
}

impl NonceCache {
	/// Create new cache with a limit of `limit` entries.
	pub fn new(limit: usize) -> Self {
		NonceCache {
			nonces: Arc::new(RwLock::new(HashMap::with_capacity(limit / 2))),
			limit,
		}
	}

	/// Retrieve a cached nonce for given sender.
	pub fn get(&self, sender: &Address) -> Option<U256> {
		self.nonces.read().get(sender).cloned()
	}

	/// Clear all entries from the cache.
	pub fn clear(&self) {
		self.nonces.write().clear();
	}
}

/// Blockchain accesss for transaction pool.
pub struct PoolClient<'a, C: 'a> {
	chain: &'a C,
	cached_nonces: CachedNonceClient<'a, C>,
	engine: &'a EthEngine,
	accounts: &'a LocalAccounts,
	best_block_header: Header,
	service_transaction_checker: Option<&'a ServiceTransactionChecker>,
}

impl<'a, C: 'a> Clone for PoolClient<'a, C> {
	fn clone(&self) -> Self {
		PoolClient {
			chain: self.chain,
			cached_nonces: self.cached_nonces.clone(),
			engine: self.engine,
			accounts: self.accounts.clone(),
			best_block_header: self.best_block_header.clone(),
			service_transaction_checker: self.service_transaction_checker.clone(),
		}
	}
}

impl<'a, C: 'a> PoolClient<'a, C>
where
	C: BlockInfo + CallContract + TransactionInfo,
{
	/// Creates new client given chain, nonce cache, accounts and service transaction verifier.
	pub fn new(
		chain: &'a C,
		cache: &'a NonceCache,
		engine: &'a EthEngine,
		accounts: &'a LocalAccounts,
		service_transaction_checker: Option<&'a ServiceTransactionChecker>,
	) -> Self {
		let best_block_header = chain.best_block_header();
		PoolClient {
			chain,
			cached_nonces: CachedNonceClient::new(chain, cache),
			engine,
			accounts,
			best_block_header,
			service_transaction_checker,
		}
	}

	/// Verifies if signed transaction is executable.
	///
	/// This should perform any verifications that rely on chain status.
	pub fn verify_signed(&self, tx: &SignedTransaction) -> Result<(), transaction::Error> {
		self.engine
			.machine()
			.verify_transaction(&tx, &self.best_block_header, self.chain)
	}
}

impl<'a, C: 'a> fmt::Debug for PoolClient<'a, C> {
	fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
		write!(fmt, "PoolClient")
	}
}

impl<'a, C: 'a> pool::client::Client for PoolClient<'a, C>
where
	C: miner::TransactionVerifierClient + Sync,
{
	fn transaction_already_included(&self, hash: &H256) -> bool {
		self.chain
			.transaction_block(TransactionId::Hash(*hash))
			.is_some()
	}

	fn verify_private_transaction_basic(
		&self,
		t: &UnverifiedTransaction,
	) -> Result<(), transaction::Error> {
		// First, check whether the nullifiers have been used.
		for ref nullifier in t.get_nullifier_set() {
			if self
				.chain
				.nullifier_transaction(nullifier)
				.is_some()
			{
				return Err(transaction::Error::ConflictNullifier);
			}
		}

		// Second, check whether the anchors are valid.
		for ref anchor in t.get_commitment_anchors() {
			if self
				.chain
				.commitment_root_block(&Node::new(anchor.into_repr()))
				.is_none()
			{
				return Err(transaction::Error::InvalidCommitmentAnchor);
			}
		}

		Ok(())
	}

	fn verify_transaction(
		&self,
		tx: UnverifiedTransaction,
	) -> Result<SignedTransaction, transaction::Error> {
		self.engine
			.verify_transaction_basic(&tx, &self.best_block_header)?;
		let tx = self
			.engine
			.verify_transaction_unordered(tx, &self.best_block_header)?;

		self.verify_signed(&tx)?;

		Ok(tx)
	}

	fn required_gas(&self, tx: &transaction::Transaction) -> U256 {
		tx.gas_required(&self.chain.latest_schedule()).into()
	}

	fn account_details(&self, address: &Address) -> pool::client::AccountDetails {
		pool::client::AccountDetails {
			nonce: self.cached_nonces.account_nonce(address),
			balance: self.chain.latest_balance(address),
			is_local: self.accounts.is_local(address),
		}
	}

	fn transaction_type(&self, tx: &SignedTransaction) -> pool::client::TransactionType {
		match self.service_transaction_checker {
			None => pool::client::TransactionType::Regular,
			Some(ref checker) => match checker.check(self.chain, &tx) {
				Ok(true) => pool::client::TransactionType::Service,
				Ok(false) => pool::client::TransactionType::Regular,
				Err(e) => {
					debug!(target: "txqueue", "Unable to verify service transaction: {:?}", e);
					pool::client::TransactionType::Regular
				}
			},
		}
	}

	fn decode_transaction(
		&self,
		transaction: &[u8],
	) -> Result<UnverifiedTransaction, transaction::Error> {
		self.engine.decode_transaction(transaction)
	}
}

impl<'a, C: 'a> NonceClient for PoolClient<'a, C>
where
	C: Nonce + Sync,
{
	fn account_nonce(&self, address: &Address) -> U256 {
		self.cached_nonces.account_nonce(address)
	}
}

impl<'a, C: 'a> NullifierClient for PoolClient<'a, C>
	where
		C: TransactionInfo + Sync,
{
	fn nullifier_exists(&self, nullifier: &U256) -> bool {
		self
			.chain
			.nullifier_transaction(nullifier)
			.is_some()
	}
}

pub(crate) struct CachedNonceClient<'a, C: 'a> {
	client: &'a C,
	cache: &'a NonceCache,
}

impl<'a, C: 'a> Clone for CachedNonceClient<'a, C> {
	fn clone(&self) -> Self {
		CachedNonceClient {
			client: self.client,
			cache: self.cache,
		}
	}
}

impl<'a, C: 'a> fmt::Debug for CachedNonceClient<'a, C> {
	fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
		fmt.debug_struct("CachedNonceClient")
			.field("cache", &self.cache.nonces.read().len())
			.field("limit", &self.cache.limit)
			.finish()
	}
}

impl<'a, C: 'a> CachedNonceClient<'a, C> {
	pub fn new(client: &'a C, cache: &'a NonceCache) -> Self {
		CachedNonceClient { client, cache }
	}
}

impl<'a, C: 'a> NonceClient for CachedNonceClient<'a, C>
where
	C: Nonce + Sync,
{
	fn account_nonce(&self, address: &Address) -> U256 {
		if let Some(nonce) = self.cache.nonces.read().get(address) {
			return *nonce;
		}

		// We don't check again if cache has been populated.
		// It's not THAT expensive to fetch the nonce from state.
		let mut cache = self.cache.nonces.write();
		let nonce = self.client.latest_nonce(address);
		cache.insert(*address, nonce);

		if cache.len() < self.cache.limit {
			return nonce;
		}

		debug!(target: "txpool", "NonceCache: reached limit.");
		trace_time!("nonce_cache:clear");

		// Remove excessive amount of entries from the cache
		let to_remove: Vec<_> = cache.keys().take(self.cache.limit / 2).cloned().collect();
		for x in to_remove {
			cache.remove(&x);
		}

		nonce
	}
}

impl<'a, C: 'a> NullifierClient for CachedNonceClient<'a, C>
	where
		C: TransactionInfo + Sync,
{
	fn nullifier_exists(&self, nullifier: &U256) -> bool {
		self.client.nullifier_transaction(nullifier).is_some()
	}
}

// Copyright 2015-2017 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

use std::sync::Arc;
use std::cmp;
use std::collections::HashMap;
use std::collections::hash_map::Entry;

use bytes::Bytes;
use ethcore_miner::pool;
use ethereum_types::{H256, U256, Address};
use heapsize::HeapSizeOf;
use ethkey::Signature;
use messages::PrivateTransaction;
use parking_lot::RwLock;
use transaction::{UnverifiedTransaction, SignedTransaction};
use txpool;
use txpool::{VerifiedTransaction, Verifier};
use error::{Error, ErrorKind};

type Pool = txpool::Pool<VerifiedPrivateTransaction, PrivateScorying>;

/// Maximum length for private transactions queues.
const MAX_QUEUE_LEN: usize = 8312;
/// Transaction with the same (sender, nonce) can be replaced only if
/// `new_gas_price > old_gas_price + old_gas_price >> SHIFT`
const GAS_PRICE_BUMP_SHIFT: usize = 3; // 2 = 25%, 3 = 12.5%, 4 = 6.25%

/// Desriptor for private transaction stored in queue for verification
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedPrivateTransaction {
	/// Original private transaction
	pub private_transaction: PrivateTransaction,
	/// Address that should be used for verification
	pub validator_account: Option<Address>,
	/// Resulted verified
	pub transaction: SignedTransaction,
	/// Original transaction's hash
	pub transaction_hash: H256,
	/// Original transaction's sender
	pub transaction_sender: Address,
}

impl txpool::VerifiedTransaction for VerifiedPrivateTransaction {
	type Hash = H256;
	type Sender = Address;

	fn hash(&self) -> &H256 {
		&self.transaction_hash
	}

	fn mem_usage(&self) -> usize {
		self.transaction.heap_size_of_children()
	}

	fn sender(&self) -> &Address {
		&self.transaction_sender
	}
}

#[derive(Debug)]
pub struct PrivateScorying;

impl txpool::Scoring<VerifiedPrivateTransaction> for PrivateScorying {
	type Score = U256;
	type Event = ();

	fn compare(&self, old: &VerifiedPrivateTransaction, other: &VerifiedPrivateTransaction) -> cmp::Ordering {
		old.transaction.nonce.cmp(&other.transaction.nonce)
	}

	fn choose(&self, old: &VerifiedPrivateTransaction, new: &VerifiedPrivateTransaction) -> txpool::scoring::Choice {
		if old.transaction.nonce != new.transaction.nonce {
			return txpool::scoring::Choice::InsertNew
		}

		let old_gp = old.transaction.gas_price;
		let new_gp = new.transaction.gas_price;

		let min_required_gp = old_gp + (old_gp >> GAS_PRICE_BUMP_SHIFT);

		match min_required_gp.cmp(&new_gp) {
			cmp::Ordering::Greater => txpool::scoring::Choice::RejectNew,
			_ => txpool::scoring::Choice::ReplaceOld,
		}
	}

	fn update_scores(&self, txs: &[txpool::Transaction<VerifiedPrivateTransaction>], scores: &mut [U256], change: txpool::scoring::Change) {
		use self::txpool::scoring::Change;

		match change {
			Change::Culled(_) => {},
			Change::RemovedAt(_) => {}
			Change::InsertedAt(i) | Change::ReplacedAt(i) => {
				assert!(i < txs.len());
				assert!(i < scores.len());

				scores[i] = txs[i].transaction.transaction.gas_price;
			},
			Change::Event(_) => {}
		}
	}

	fn should_replace(&self, old: &VerifiedPrivateTransaction, new: &VerifiedPrivateTransaction) -> bool {
		if old.sender() == new.sender() {
			// prefer earliest transaction
			if new.transaction.nonce < old.transaction.nonce {
				return true
			}
		}

		self.choose(old, new) == txpool::scoring::Choice::ReplaceOld
	}
}

/// Checks readiness of transactions by comparing the nonce to state nonce.
/// Guarantees only one transaction per sender
#[derive(Debug)]
pub struct PrivateReadyState<C> {
	nonces: HashMap<Address, U256>,
	state: C,
}

impl<C> PrivateReadyState<C> {
	/// Create new State checker, given client interface.
	pub fn new(
		state: C,
	) -> Self {
		PrivateReadyState {
			nonces: Default::default(),
			state,
		}
	}
}

impl<C: pool::client::NonceClient> txpool::Ready<VerifiedPrivateTransaction> for PrivateReadyState<C> {
	fn is_ready(&mut self, tx: &VerifiedPrivateTransaction) -> txpool::Readiness {
		let sender = tx.sender();
		let state = &self.state;
		let state_nonce = state.account_nonce(sender);
		match self.nonces.entry(*sender) {
			Entry::Vacant(entry) => {
				let nonce = entry.insert(state_nonce);
				match tx.transaction.nonce.cmp(nonce) {
					cmp::Ordering::Greater => txpool::Readiness::Future,
					cmp::Ordering::Less => txpool::Readiness::Stale,
					cmp::Ordering::Equal => {
						*nonce = *nonce + 1.into();
						txpool::Readiness::Ready
					},
				}
			}
			Entry::Occupied(_) => {
				txpool::Readiness::Future
			}
		}
	}
}

/// Storage for private transactions for verification
pub struct VerificationStore {
	verification_pool: RwLock<Pool>,
	verification_options: pool::verifier::Options,
}

impl Default for VerificationStore {
	fn default() -> Self {
		VerificationStore {
			verification_pool: RwLock::new(
				txpool::Pool::new(
					txpool::NoopListener,
					PrivateScorying,
					pool::Options {
						max_count: MAX_QUEUE_LEN,
						max_per_sender: MAX_QUEUE_LEN / 10,
						max_mem_usage: 8 * 1024 * 1024,
					},
				)
			),
			verification_options: pool::verifier::Options {
				// TODO [ToDr] This should probably be based on some real values?
				minimal_gas_price: 0.into(),
				block_gas_limit: 8_000_000.into(),
				tx_gas_limit: U256::max_value(),
			},
		}
	}
}

impl VerificationStore {
	/// Adds private transaction for verification into the store
	pub fn add_transaction<C: pool::client::Client>(
		&self,
		transaction: UnverifiedTransaction,
		validator_account: Option<Address>,
		private_transaction: PrivateTransaction,
		client: C,
	) -> Result<(), Error> {

		let options = self.verification_options.clone();
		// Use pool's verifying pipeline for original transaction's verification
		let verifier = pool::verifier::Verifier::new(client, options, Default::default());
		let _verified_tx = verifier.verify_transaction(pool::verifier::Transaction::Unverified(transaction.clone()))?;
		let signed_tx = SignedTransaction::new(transaction)?;
		let verified = VerifiedPrivateTransaction {
			private_transaction,
			validator_account,
			transaction: signed_tx.clone(),
			transaction_hash: signed_tx.hash(),
			transaction_sender: signed_tx.sender(),
		};
		let mut pool = self.verification_pool.write();
		pool.import(verified)?;
		Ok(())
	}

	/// Drains transactions ready for verification from the pool
	/// Returns only one transaction per sender because several cannot be verified in a row without verification from other peers
	pub fn drain<C: pool::client::NonceClient>(&self, client: C) -> Vec<Arc<VerifiedPrivateTransaction>> {
		let ready = PrivateReadyState::new(client);
		let mut hashes: Vec<H256> = Vec::new();
		let res: Vec<Arc<VerifiedPrivateTransaction>> = self.verification_pool.read().pending(ready).collect();
		res
			.iter()
			.for_each(|tx| {
				hashes.push(tx.hash().clone());
			}
		);
		let mut pool = self.verification_pool.write();
		hashes
			.iter()
			.for_each(|hash| {
				pool.remove(&hash, true);
			}
		);
		res
	}
}

/// Desriptor for private transaction stored in queue for signing
#[derive(Debug, Clone)]
pub struct PrivateTransactionSigningDesc {
	/// Original unsigned transaction
	pub original_transaction: SignedTransaction,
	/// Supposed validators from the contract
	pub validators: Vec<Address>,
	/// Already obtained signatures
	pub received_signatures: Vec<Signature>,
	/// State after transaction execution to compare further with received from validators
	pub state: Bytes,
	/// Build-in nonce of the contract
	pub contract_nonce: U256,
}

/// Storage for private transactions for signing
#[derive(Default)]
pub struct SigningStore {
	/// Transactions and descriptors for signing
	transactions: HashMap<H256, PrivateTransactionSigningDesc>,
}

impl SigningStore {
	/// Adds new private transaction into the store for signing
	pub fn add_transaction(
		&mut self,
		private_hash: H256,
		transaction: SignedTransaction,
		validators: Vec<Address>,
		state: Bytes,
		contract_nonce: U256,
	) -> Result<(), Error> {
		if self.transactions.len() > MAX_QUEUE_LEN {
			bail!(ErrorKind::QueueIsFull);
		}

		self.transactions.insert(private_hash, PrivateTransactionSigningDesc {
			original_transaction: transaction.clone(),
			validators: validators.clone(),
			received_signatures: Vec::new(),
			state,
			contract_nonce,
		});
		Ok(())
	}

	/// Get copy of private transaction's description from the storage
	pub fn get(&self, private_hash: &H256) -> Option<PrivateTransactionSigningDesc> {
		self.transactions.get(private_hash).cloned()
	}

	/// Removes desc from the store (after verification is completed)
	pub fn remove(&mut self, private_hash: &H256) -> Result<(), Error> {
		self.transactions.remove(private_hash);
		Ok(())
	}

	/// Adds received signature for the stored private transaction
	pub fn add_signature(&mut self, private_hash: &H256, signature: Signature) -> Result<(), Error> {
		let desc = self.transactions.get_mut(private_hash).ok_or_else(|| ErrorKind::PrivateTransactionNotFound)?;
		if !desc.received_signatures.contains(&signature) {
			desc.received_signatures.push(signature);
		}
		Ok(())
	}
}

use std::ops::Range;
use std::{
    cmp,
    collections::{BTreeMap, BTreeSet, HashMap},
    mem,
};

use alloy::primitives::address;
use alloy_compat::Compat as _;
use anyhow::{anyhow, bail, ensure, Context as _};
use ethereum_types::{Address, U256};
use evm_arithmetization::{
    generation::{mpt::AccountRlp, TrieInputs},
    proof::TrieRoots,
    testing_utils::{BEACON_ROOTS_CONTRACT_ADDRESS, HISTORY_BUFFER_LENGTH},
    GenerationInputs,
};
use itertools::Itertools as _;
use keccak_hash::H256;
use mpt_trie::partial_trie::PartialTrie as _;
use nunny::NonEmpty;
use zk_evm_common::gwei_to_wei;

use crate::{
    typed_mpt::{ReceiptTrie, StateMpt, StateTrie, StorageTrie, TransactionTrie, TrieKey},
    BlockLevelData, BlockTrace, BlockTraceTriePreImages, CombinedPreImages, ContractCodeUsage,
    Field, OtherBlockData, SeparateStorageTriesPreImage, SeparateTriePreImage,
    SeparateTriePreImages, TxnInfo, TxnMeta, TxnTrace,
};

/// TODO(0xaatif): document this after https://github.com/0xPolygonZero/zk_evm/issues/275
pub fn entrypoint(
    trace: BlockTrace,
    other: OtherBlockData,
    batch_size_hint: usize,
) -> anyhow::Result<Vec<GenerationInputs<Field>>> {
    ensure!(batch_size_hint != 0);

    let BlockTrace {
        trie_pre_images,
        code_db,
        txn_info,
    } = trace;
    let (state, storage, mut code) = start(trie_pre_images)?;
    code.extend(code_db);

    let OtherBlockData {
        b_data:
            BlockLevelData {
                b_meta,
                b_hashes,
                mut withdrawals,
            },
        checkpoint_state_trie_root,
        checkpoint_consolidated_hash,
        burn_addr,
    } = other;

    for (_, amt) in &mut withdrawals {
        *amt = gwei_to_wei(*amt)
    }

    let batches = middle(
        state,
        storage,
        batch(txn_info, batch_size_hint),
        &mut code,
        b_meta.block_timestamp,
        b_meta.parent_beacon_block_root,
        withdrawals,
    )?;

    let mut running_gas_used = 0;
    Ok(batches
        .into_iter()
        .map(
            |Batch {
                 first_txn_ix,
                 gas_used,
                 contract_code,
                 byte_code,
                 before:
                     IntraBlockTries {
                         state,
                         storage,
                         transaction,
                         receipt,
                     },
                 after,
                 withdrawals,
             }| GenerationInputs::<Field> {
                txn_number_before: first_txn_ix.into(),
                gas_used_before: running_gas_used.into(),
                gas_used_after: {
                    running_gas_used += gas_used;
                    running_gas_used.into()
                },
                signed_txns: byte_code.into_iter().map(Into::into).collect(),
                withdrawals,
                ger_data: None,
                tries: TrieInputs {
                    state_trie: state.into(),
                    transactions_trie: transaction.into(),
                    receipts_trie: receipt.into(),
                    storage_tries: storage.into_iter().map(|(k, v)| (k, v.into())).collect(),
                },
                trie_roots_after: after,
                checkpoint_state_trie_root,
                checkpoint_consolidated_hash,
                contract_code: contract_code
                    .into_iter()
                    .map(|it| (keccak_hash::keccak(&it), it))
                    .collect(),
                block_metadata: b_meta.clone(),
                block_hashes: b_hashes.clone(),
                burn_addr,
            },
        )
        .collect())
}

/// The user has either provided us with a [`serde`]-ed
/// [`HashedPartialTrie`](mpt_trie::partial_trie::HashedPartialTrie),
/// or a [`wire`](crate::wire)-encoded representation of one.
///
/// Turn either of those into our [`typed_mpt`](crate::typed_mpt)
/// representations.
fn start(
    pre_images: BlockTraceTriePreImages,
) -> anyhow::Result<(StateMpt, BTreeMap<H256, StorageTrie>, Hash2Code)> {
    Ok(match pre_images {
        // TODO(0xaatif): https://github.com/0xPolygonZero/zk_evm/issues/401
        //                refactor our convoluted input types
        BlockTraceTriePreImages::Separate(SeparateTriePreImages {
            state: SeparateTriePreImage::Direct(state),
            storage: SeparateStorageTriesPreImage::MultipleTries(storage),
        }) => {
            let state = state.items().try_fold(
                StateMpt::default(),
                |mut acc, (nibbles, hash_or_val)| {
                    let path = TrieKey::from_nibbles(nibbles);
                    match hash_or_val {
                        mpt_trie::trie_ops::ValOrHash::Val(bytes) => {
                            #[expect(deprecated)] // this is MPT specific
                            acc.insert_by_hashed_address(
                                path.into_hash()
                                    .context("invalid path length in direct state trie")?,
                                rlp::decode(&bytes)
                                    .context("invalid AccountRlp in direct state trie")?,
                            )?;
                        }
                        mpt_trie::trie_ops::ValOrHash::Hash(h) => {
                            acc.insert_hash_by_key(path, h)?;
                        }
                    };
                    anyhow::Ok(acc)
                },
            )?;
            let storage = storage
                .into_iter()
                .map(|(k, SeparateTriePreImage::Direct(v))| {
                    v.items()
                        .try_fold(StorageTrie::default(), |mut acc, (nibbles, hash_or_val)| {
                            let path = TrieKey::from_nibbles(nibbles);
                            match hash_or_val {
                                mpt_trie::trie_ops::ValOrHash::Val(value) => {
                                    acc.insert(path, value)?;
                                }
                                mpt_trie::trie_ops::ValOrHash::Hash(h) => {
                                    acc.insert_hash(path, h)?;
                                }
                            };
                            anyhow::Ok(acc)
                        })
                        .map(|v| (k, v))
                })
                .collect::<Result<_, _>>()?;
            (state, storage, Hash2Code::new())
        }
        BlockTraceTriePreImages::Combined(CombinedPreImages { compact }) => {
            let instructions = crate::wire::parse(&compact)
                .context("couldn't parse instructions from binary format")?;
            let crate::type1::Frontend {
                state,
                storage,
                code,
            } = crate::type1::frontend(instructions)?;
            (state, storage, code.into_iter().map(Into::into).collect())
        }
    })
}

/// Break `txns` into batches of length `batch_size_hint`, prioritising creating
/// at least two batches.
///
/// [`None`] represents a dummy transaction that should not increment the
/// transaction index.
fn batch(txns: Vec<TxnInfo>, batch_size_hint: usize) -> Vec<Vec<Option<TxnInfo>>> {
    let hint = cmp::max(batch_size_hint, 1);
    let mut txns = txns.into_iter().map(Some).collect::<Vec<_>>();
    let n_batches = txns.iter().chunks(hint).into_iter().count();
    match (txns.len(), n_batches) {
        // enough
        (_, 2..) => txns
            .into_iter()
            .chunks(hint)
            .into_iter()
            .map(FromIterator::from_iter)
            .collect(),
        // not enough batches at `hint`, but enough real transactions,
        // so just split them in half
        (2.., ..2) => {
            let second = txns.split_off(txns.len() / 2);
            vec![txns, second]
        }
        // add padding
        (0 | 1, _) => txns
            .into_iter()
            .pad_using(2, |_ix| None)
            .map(|it| vec![it])
            .collect(),
    }
}

#[test]
fn test_batch() {
    #[track_caller]
    fn do_test(n: usize, hint: usize, exp: impl IntoIterator<Item = usize>) {
        itertools::assert_equal(
            exp,
            batch(vec![TxnInfo::default(); n], hint)
                .iter()
                .map(Vec::len),
        )
    }

    do_test(0, 0, [1, 1]); // pad2
    do_test(1, 0, [1, 1]); // pad1
    do_test(2, 0, [1, 1]); // exact
    do_test(3, 0, [1, 1, 1]);
    do_test(3, 1, [1, 1, 1]);
    do_test(3, 2, [2, 1]); // leftover after hint
    do_test(3, 3, [1, 2]); // big hint
}

#[derive(Debug)]
struct Batch<StateTrieT> {
    pub first_txn_ix: usize,
    pub gas_used: u64,
    /// See [`GenerationInputs::contract_code`].
    pub contract_code: BTreeSet<Vec<u8>>,
    /// For each transaction in batch, in order.
    pub byte_code: Vec<NonEmpty<Vec<u8>>>,

    pub before: IntraBlockTries<StateTrieT>,
    pub after: TrieRoots,

    /// Empty for all but the final batch
    pub withdrawals: Vec<(Address, U256)>,
}

/// [`evm_arithmetization::generation::TrieInputs`],
/// generic over state trie representation.
#[derive(Debug)]
struct IntraBlockTries<StateTrieT> {
    pub state: StateTrieT,
    pub storage: BTreeMap<H256, StorageTrie>,
    pub transaction: TransactionTrie,
    pub receipt: ReceiptTrie,
}

/// Does the main work mentioned in the [module documentation](super).
fn middle<StateTrieT: StateTrie + Clone>(
    // state at the beginning of the block
    mut state_trie: StateTrieT,
    // storage at the beginning of the block
    mut storage_tries: BTreeMap<H256, StorageTrie>,
    // None represents a dummy transaction that should not increment the transaction index
    // all batches SHOULD not be empty
    batches: Vec<Vec<Option<TxnInfo>>>,
    code: &mut Hash2Code,
    block_timestamp: U256,
    parent_beacon_block_root: H256,
    // added to final batch
    mut withdrawals: Vec<(Address, U256)>,
) -> anyhow::Result<Vec<Batch<StateTrieT>>> {
    // Initialise the storage tries.
    for (haddr, acct) in state_trie.iter() {
        let storage = storage_tries.entry(haddr).or_insert({
            let mut it = StorageTrie::default();
            it.insert_hash(TrieKey::default(), acct.storage_root)
                .expect("empty trie insert cannot fail");
            it
        });
        ensure!(
            storage.root() == acct.storage_root,
            "inconsistent initial storage for hashed address {haddr:x}"
        )
    }

    // These are the per-block tries.
    let mut transaction_trie = TransactionTrie::new();
    let mut receipt_trie = ReceiptTrie::new();

    let mut out = vec![];

    let mut txn_ix = 0; // incremented for non-dummy transactions
    let mut loop_ix = 0; // always incremented
    let loop_len = batches.iter().flatten().count();
    for batch in batches {
        let batch_first_txn_ix = txn_ix; // GOTCHA: if there are no transactions in this batch
        let mut batch_gas_used = 0;
        let mut batch_byte_code = vec![];
        let mut batch_contract_code = BTreeSet::from([vec![]]); // always include empty code

        let mut before = IntraBlockTries {
            state: state_trie.clone(),
            transaction: transaction_trie.clone(),
            receipt: receipt_trie.clone(),
            storage: storage_tries.clone(),
        };

        // We want to perform mask the TrieInputs above,
        // but won't know the bounds until after the loop below,
        // so store that information here.
        let mut storage_masks = BTreeMap::<_, BTreeSet<TrieKey>>::new();
        let mut state_mask = BTreeSet::new();

        if txn_ix == 0 {
            do_beacon_hook(
                block_timestamp,
                &mut storage_tries,
                &mut storage_masks,
                parent_beacon_block_root,
                &mut state_mask,
                &mut state_trie,
            )?;
        }

        for txn in batch {
            let do_increment_txn_ix = txn.is_some();
            let TxnInfo {
                traces,
                meta:
                    TxnMeta {
                        byte_code,
                        new_receipt_trie_node_byte,
                        gas_used: txn_gas_used,
                    },
            } = txn.unwrap_or_default();

            if let Ok(nonempty) = nunny::Vec::new(byte_code) {
                batch_byte_code.push(nonempty.clone());
                transaction_trie.insert(txn_ix, nonempty.into())?;
                receipt_trie.insert(
                    txn_ix,
                    map_receipt_bytes(new_receipt_trie_node_byte.clone())?,
                )?;
            }

            batch_gas_used += txn_gas_used;

            for (
                addr,
                just_access,
                TxnTrace {
                    balance,
                    nonce,
                    storage_read,
                    storage_written,
                    code_usage,
                    self_destructed,
                },
            ) in traces
                .into_iter()
                .map(|(addr, trc)| (addr, trc == TxnTrace::default(), trc))
            {
                let (_, _, receipt) = evm_arithmetization::generation::mpt::decode_receipt(
                    &map_receipt_bytes(new_receipt_trie_node_byte.clone())?,
                )
                .map_err(|e| anyhow!("{e:?}"))
                .context("couldn't decode receipt")?;

                let (mut acct, born) = state_trie
                    .get_by_address(addr)
                    .map(|acct| (acct, false))
                    .unwrap_or((AccountRlp::default(), true));

                if born || just_access {
                    state_trie
                        .clone()
                        .insert_by_address(addr, acct)
                        .context(format!(
                            "couldn't reach state of {} address {addr:x}",
                            match born {
                                true => "created",
                                false => "accessed",
                            }
                        ))?;
                }

                let do_writes = !just_access
                    && match born {
                        // if txn failed, don't commit changes to trie
                        true => receipt.status,
                        false => true,
                    };

                let storage_mask = storage_masks.entry(addr).or_default();

                storage_mask.extend(
                    storage_written
                        .keys()
                        .chain(&storage_read)
                        .map(|it| TrieKey::from_hash(keccak_hash::keccak(it))),
                );

                if do_writes {
                    acct.balance = balance.unwrap_or(acct.balance);
                    acct.nonce = nonce.unwrap_or(acct.nonce);
                    acct.code_hash = code_usage
                        .map(|it| match it {
                            ContractCodeUsage::Read(hash) => {
                                batch_contract_code.insert(code.get(hash)?);
                                anyhow::Ok(hash)
                            }
                            ContractCodeUsage::Write(bytes) => {
                                code.insert(bytes.clone());
                                let hash = keccak_hash::keccak(&bytes);
                                batch_contract_code.insert(bytes);
                                Ok(hash)
                            }
                        })
                        .transpose()?
                        .unwrap_or(acct.code_hash);

                    if !storage_written.is_empty() {
                        let storage = match born {
                            true => storage_tries.entry(keccak_hash::keccak(addr)).or_default(),
                            false => storage_tries
                                .get_mut(&keccak_hash::keccak(addr))
                                .context(format!("missing storage trie for address {addr:x}"))?,
                        };

                        for (k, v) in storage_written {
                            let slot = TrieKey::from_hash(keccak_hash::keccak(k));
                            match v.is_zero() {
                                // this is actually a delete
                                true => storage_mask.extend(storage.reporting_remove(slot)?),
                                false => {
                                    storage.insert(slot, rlp::encode(&v).to_vec())?;
                                }
                            }
                        }
                        acct.storage_root = storage.root();
                    }

                    state_trie.insert_by_address(addr, acct)?;
                    state_mask.insert(TrieKey::from_address(addr));
                } else {
                    // Simple state access
                    const PRECOMPILE_ADDRESSES: Range<alloy::primitives::Address> =
                        address!("0000000000000000000000000000000000000001")
                            ..address!("000000000000000000000000000000000000000a");

                    if receipt.status || !PRECOMPILE_ADDRESSES.contains(&addr.compat()) {
                        // TODO(0xaatif): https://github.com/0xPolygonZero/zk_evm/pull/613
                        //                masking like this SHOULD be a space-saving optimization,
                        //                BUT if it's omitted, we actually get state root mismatches
                        state_mask.insert(TrieKey::from_address(addr));
                    }
                }

                if self_destructed {
                    storage_tries.remove(&keccak_hash::keccak(addr));
                    state_mask.extend(state_trie.reporting_remove(addr)?)
                }
            }

            if do_increment_txn_ix {
                txn_ix += 1;
            }
            loop_ix += 1;
        } // txn in batch

        out.push(Batch {
            first_txn_ix: batch_first_txn_ix,
            gas_used: batch_gas_used,
            contract_code: batch_contract_code,
            byte_code: batch_byte_code,
            withdrawals: match loop_ix == loop_len {
                true => {
                    for (addr, amt) in &withdrawals {
                        state_mask.insert(TrieKey::from_address(*addr));
                        let mut acct = state_trie
                            .get_by_address(*addr)
                            .context("missing address for withdrawal")?;
                        acct.balance += *amt;
                        state_trie
                            .insert_by_address(*addr, acct)
                            // TODO(0xaatif): https://github.com/0xPolygonZero/zk_evm/issues/275
                            //                Add an entry API
                            .expect("insert must succeed with the same key as a successful `get`");
                    }
                    mem::take(&mut withdrawals)
                }
                false => vec![],
            },
            before: {
                before.state.mask(state_mask)?;
                before.receipt.mask(batch_first_txn_ix..txn_ix)?;
                before.transaction.mask(batch_first_txn_ix..txn_ix)?;

                let keep = storage_masks
                    .keys()
                    .map(keccak_hash::keccak)
                    .collect::<BTreeSet<_>>();
                before.storage.retain(|haddr, _| keep.contains(haddr));

                for (addr, mask) in storage_masks {
                    if let Some(it) = before.storage.get_mut(&keccak_hash::keccak(addr)) {
                        it.mask(mask)?
                    } // else must have self-destructed
                }
                before
            },
            after: TrieRoots {
                state_root: state_trie.root(),
                transactions_root: transaction_trie.root(),
                receipts_root: receipt_trie.root(),
            },
        });
    } // batch in batches

    Ok(out)
}

/// Updates the storage of the beacon block root contract,
/// according to <https://eips.ethereum.org/EIPS/eip-4788>
///
/// This is cancun-specific, and runs at the start of the block,
/// before any transactions (as per the EIP).
fn do_beacon_hook<StateTrieT: StateTrie + Clone>(
    block_timestamp: U256,
    storage: &mut BTreeMap<H256, StorageTrie>,
    trim_storage: &mut BTreeMap<ethereum_types::H160, BTreeSet<TrieKey>>,
    parent_beacon_block_root: H256,
    trim_state: &mut BTreeSet<TrieKey>,
    state_trie: &mut StateTrieT,
) -> anyhow::Result<()> {
    let history_timestamp = block_timestamp % HISTORY_BUFFER_LENGTH.value;
    let history_timestamp_next = history_timestamp + HISTORY_BUFFER_LENGTH.value;
    let beacon_storage = storage
        .get_mut(&keccak_hash::keccak(BEACON_ROOTS_CONTRACT_ADDRESS))
        .context("missing beacon contract storage trie")?;
    let beacon_trim = trim_storage
        .entry(BEACON_ROOTS_CONTRACT_ADDRESS)
        .or_default();
    for (ix, u) in [
        (history_timestamp, block_timestamp),
        (
            history_timestamp_next,
            U256::from_big_endian(parent_beacon_block_root.as_bytes()),
        ),
    ] {
        let mut h = [0; 32];
        ix.to_big_endian(&mut h);
        let slot = TrieKey::from_hash(keccak_hash::keccak(H256::from_slice(&h)));
        beacon_trim.insert(slot);

        match u.is_zero() {
            true => beacon_trim.extend(beacon_storage.reporting_remove(slot)?),
            false => {
                beacon_storage.insert(slot, alloy::rlp::encode(u.compat()))?;
                beacon_trim.insert(slot);
            }
        }
    }
    trim_state.insert(TrieKey::from_address(BEACON_ROOTS_CONTRACT_ADDRESS));
    let mut beacon_acct = state_trie
        .get_by_address(BEACON_ROOTS_CONTRACT_ADDRESS)
        .context("missing beacon contract address")?;
    beacon_acct.storage_root = beacon_storage.root();
    state_trie
        .insert_by_address(BEACON_ROOTS_CONTRACT_ADDRESS, beacon_acct)
        // TODO(0xaatif): https://github.com/0xPolygonZero/zk_evm/issues/275
        //                Add an entry API
        .expect("insert must succeed with the same key as a successful `get`");
    Ok(())
}

fn map_receipt_bytes(bytes: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    match rlp::decode::<evm_arithmetization::generation::mpt::LegacyReceiptRlp>(&bytes) {
        Ok(_) => Ok(bytes),
        Err(_) => {
            rlp::decode(&bytes).context("couldn't decode receipt as a legacy receipt or raw bytes")
        }
    }
}

/// Code hash mappings that we have constructed from parsing the block
/// trace.
/// If there are any txns that create contracts, then they will also
/// get added here as we process the deltas.
struct Hash2Code {
    /// Key must always be [`hash`] of value.
    inner: HashMap<H256, Vec<u8>>,
}

impl Hash2Code {
    pub fn new() -> Self {
        let mut this = Self {
            inner: HashMap::new(),
        };
        this.insert(vec![]);
        this
    }
    pub fn get(&mut self, hash: H256) -> anyhow::Result<Vec<u8>> {
        match self.inner.get(&hash) {
            Some(code) => Ok(code.clone()),
            None => bail!("no code for hash {}", hash),
        }
    }
    pub fn insert(&mut self, code: Vec<u8>) {
        self.inner.insert(keccak_hash::keccak(&code), code);
    }
}

impl Extend<Vec<u8>> for Hash2Code {
    fn extend<II: IntoIterator<Item = Vec<u8>>>(&mut self, iter: II) {
        for it in iter {
            self.insert(it)
        }
    }
}

impl FromIterator<Vec<u8>> for Hash2Code {
    fn from_iter<II: IntoIterator<Item = Vec<u8>>>(iter: II) -> Self {
        let mut this = Self::new();
        this.extend(iter);
        this
    }
}
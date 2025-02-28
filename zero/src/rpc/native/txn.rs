use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use alloy::{
    primitives::{keccak256, Address, B256, U256},
    providers::{
        ext::DebugApi as _,
        network::{eip2718::Encodable2718, Ethereum, Network},
        Provider,
    },
    rpc::types::{
        eth::{AccessList, Block, Transaction},
        trace::geth::{
            AccountState, DiffMode, GethDebugBuiltInTracerType, GethDebugTracerType,
            GethDebugTracingOptions, GethTrace, PreStateConfig, PreStateFrame, PreStateMode,
        },
    },
    transports::Transport,
};
use alloy_compat::Compat;
use anyhow::Context as _;
use futures::stream::{FuturesOrdered, TryStreamExt};
use trace_decoder::{ContractCodeUsage, TxnInfo, TxnMeta, TxnTrace};

use super::CodeDb;

/// Processes the transactions in the given block and updates the code db.
pub(super) async fn process_transactions<ProviderT, TransportT>(
    block: &Block,
    provider: &ProviderT,
) -> anyhow::Result<(CodeDb, Vec<TxnInfo>)>
where
    ProviderT: Provider<TransportT>,
    TransportT: Transport + Clone,
{
    block
        .transactions
        .as_transactions()
        .context("No transactions in block")?
        .iter()
        .map(|tx| process_transaction(provider, tx))
        .collect::<FuturesOrdered<_>>()
        .try_fold(
            (BTreeSet::new(), Vec::new()),
            |(mut code_db, mut txn_infos), (tx_code_db, txn_info)| async move {
                code_db.extend(tx_code_db);
                txn_infos.push(txn_info);
                Ok((code_db, txn_infos))
            },
        )
        .await
}

/// Processes the transaction with the given transaction hash and updates the
/// accounts state.
async fn process_transaction<ProviderT, TransportT>(
    provider: &ProviderT,
    tx: &Transaction,
) -> anyhow::Result<(CodeDb, TxnInfo)>
where
    ProviderT: Provider<TransportT>,
    TransportT: Transport + Clone,
{
    let (tx_receipt, pre_trace, diff_trace) = fetch_tx_data(provider, &tx.hash).await?;
    let tx_status = tx_receipt.status();
    let tx_receipt = tx_receipt.map_inner(rlp::map_receipt_envelope);
    let access_list = parse_access_list(tx.access_list.as_ref());

    let tx_meta = TxnMeta {
        byte_code: <Ethereum as Network>::TxEnvelope::try_from(tx.clone())?.encoded_2718(),
        new_receipt_trie_node_byte: alloy::rlp::encode(tx_receipt.inner),
        gas_used: tx_receipt.gas_used as u64,
    };

    let (code_db, mut tx_traces) = match (pre_trace, diff_trace) {
        (
            GethTrace::PreStateTracer(PreStateFrame::Default(read)),
            GethTrace::PreStateTracer(PreStateFrame::Diff(diff)),
        ) => process_tx_traces(access_list, read, diff).await?,
        _ => unreachable!(),
    };

    // Handle case when transaction failed and a contract creation was reverted
    if !tx_status && tx_receipt.contract_address.is_some() {
        tx_traces.insert(tx_receipt.contract_address.unwrap(), TxnTrace::default());
    }

    Ok((
        code_db,
        TxnInfo {
            meta: tx_meta,
            traces: tx_traces
                .into_iter()
                .map(|(k, v)| (k.compat(), v))
                .collect(),
        },
    ))
}

/// Fetches the transaction data for the given transaction hash.
async fn fetch_tx_data<ProviderT, TransportT>(
    provider: &ProviderT,
    tx_hash: &B256,
) -> anyhow::Result<(<Ethereum as Network>::ReceiptResponse, GethTrace, GethTrace), anyhow::Error>
where
    ProviderT: Provider<TransportT>,
    TransportT: Transport + Clone,
{
    let tx_receipt_fut = provider.get_transaction_receipt(*tx_hash);
    let pre_trace_fut = provider.debug_trace_transaction(*tx_hash, prestate_tracing_options(false));
    let diff_trace_fut = provider.debug_trace_transaction(*tx_hash, prestate_tracing_options(true));

    let (tx_receipt, pre_trace, diff_trace) =
        futures::try_join!(tx_receipt_fut, pre_trace_fut, diff_trace_fut,)?;

    Ok((
        tx_receipt.context("Transaction receipt not found.")?,
        pre_trace,
        diff_trace,
    ))
}

/// Parse the access list data into a hashmap.
fn parse_access_list(access_list: Option<&AccessList>) -> HashMap<Address, HashSet<B256>> {
    let mut result = HashMap::new();

    if let Some(access_list) = access_list {
        for item in access_list.0.clone() {
            result
                .entry(item.address)
                .or_insert_with(HashSet::new)
                .extend(item.storage_keys);
        }
    }

    result
}

/// Processes the transaction traces and updates the accounts state.
async fn process_tx_traces(
    mut access_list: HashMap<Address, HashSet<B256>>,
    read_trace: PreStateMode,
    diff_trace: DiffMode,
) -> anyhow::Result<(CodeDb, BTreeMap<Address, TxnTrace>)> {
    let DiffMode {
        pre: pre_trace,
        post: post_trace,
    } = diff_trace;

    let addresses: HashSet<_> = read_trace
        .0
        .keys()
        .chain(post_trace.keys())
        .chain(pre_trace.keys())
        .chain(access_list.keys())
        .copied()
        .collect();

    let mut traces = BTreeMap::new();
    let mut code_db: CodeDb = BTreeSet::new();

    for address in addresses {
        let read_state = read_trace.0.get(&address);
        let pre_state = pre_trace.get(&address);
        let post_state = post_trace.get(&address);

        let balance = post_state.and_then(|x| x.balance);
        let (storage_read, storage_written) = process_storage(
            access_list.remove(&address).unwrap_or_default(),
            read_state,
            post_state,
            pre_state,
        );
        let code = process_code(post_state, read_state, &mut code_db).await;
        let nonce = process_nonce(post_state, &code);
        let self_destructed = process_self_destruct(post_state, pre_state);

        let result = TxnTrace {
            balance: balance.map(Compat::compat),
            nonce: nonce.map(Compat::compat),
            storage_read: storage_read.into_iter().map(Compat::compat).collect(),
            storage_written: storage_written
                .into_iter()
                .map(|(k, v)| (k.compat(), v.compat()))
                .collect(),
            code_usage: code,
            self_destructed,
        };

        traces.insert(address, result);
    }

    Ok((code_db, traces))
}

/// Processes the nonce for the given account state.
///
/// If a contract is created, the nonce is set to 1.
fn process_nonce(
    post_state: Option<&AccountState>,
    code_usage: &Option<ContractCodeUsage>,
) -> Option<U256> {
    post_state
        .and_then(|x| x.nonce.map(U256::from))
        .or_else(|| {
            if let Some(ContractCodeUsage::Write(_)) = code_usage.as_ref() {
                Some(U256::from(1))
            } else {
                None
            }
        })
}

/// Processes the self destruct for the given account state.
/// This wraps the actual boolean indicator into an `Option` so that we can skip
/// serialization of `None` values, which represent most cases.
fn process_self_destruct(
    post_state: Option<&AccountState>,
    pre_state: Option<&AccountState>,
) -> bool {
    if post_state.is_none() {
        // EIP-6780:
        // A contract is considered created at the beginning of a create
        // transaction or when a CREATE series operation begins execution (CREATE,
        // CREATE2, and other operations that deploy contracts in the future). If a
        // balance exists at the contract’s new address it is still considered to be a
        // contract creation.
        if let Some(acc) = pre_state {
            if acc.code.is_none() && acc.storage.keys().collect::<Vec<_>>().is_empty() {
                return true;
            }
        }
    }

    false
}

/// Processes the storage for the given account state.
///
/// Returns the storage read and written for the given account in the
/// transaction and updates the storage keys.
fn process_storage(
    access_list: HashSet<B256>,
    acct_state: Option<&AccountState>,
    post_acct: Option<&AccountState>,
    pre_acct: Option<&AccountState>,
) -> (BTreeSet<B256>, BTreeMap<B256, U256>) {
    let mut storage_read = BTreeSet::from_iter(access_list);
    storage_read.extend(
        acct_state
            .into_iter()
            .flat_map(|acct| acct.storage.keys().copied()),
    );

    let mut storage_written: BTreeMap<B256, U256> = post_acct
        .map(|x| {
            x.storage
                .iter()
                .map(|(k, v)| (*k, U256::from_le_bytes((*v).into())))
                .collect()
        })
        .unwrap_or_default();

    // Add the deleted keys to the storage written
    if let Some(pre_acct) = pre_acct {
        for key in pre_acct.storage.keys() {
            storage_written.entry(*key).or_default();
        }
    };

    (storage_read, storage_written)
}

/// Processes the code usage for the given account state.
async fn process_code(
    post_state: Option<&AccountState>,
    read_state: Option<&AccountState>,
    code_db: &mut CodeDb,
) -> Option<ContractCodeUsage> {
    match (
        post_state.and_then(|x| x.code.as_ref()),
        read_state.and_then(|x| x.code.as_ref()),
    ) {
        (Some(post_code), _) => {
            code_db.insert(post_code.to_vec());
            Some(ContractCodeUsage::Write(post_code.to_vec()))
        }
        (_, Some(read_code)) => {
            let code_hash = keccak256(read_code).compat();
            code_db.insert(read_code.to_vec());
            Some(ContractCodeUsage::Read(code_hash))
        }
        _ => None,
    }
}

mod rlp {
    use alloy::consensus::{Receipt, ReceiptEnvelope};
    use alloy::rpc::types::eth::ReceiptWithBloom;

    pub fn map_receipt_envelope(
        rpc: ReceiptEnvelope<alloy::rpc::types::eth::Log>,
    ) -> ReceiptEnvelope<alloy::primitives::Log> {
        match rpc {
            ReceiptEnvelope::Legacy(it) => ReceiptEnvelope::Legacy(map_receipt_with_bloom(it)),
            ReceiptEnvelope::Eip2930(it) => ReceiptEnvelope::Eip2930(map_receipt_with_bloom(it)),
            ReceiptEnvelope::Eip1559(it) => ReceiptEnvelope::Eip1559(map_receipt_with_bloom(it)),
            ReceiptEnvelope::Eip4844(it) => ReceiptEnvelope::Eip4844(map_receipt_with_bloom(it)),
            other => panic!("unsupported receipt type: {:?}", other),
        }
    }
    fn map_receipt_with_bloom(
        rpc: ReceiptWithBloom<alloy::rpc::types::eth::Log>,
    ) -> ReceiptWithBloom<alloy::primitives::Log> {
        let ReceiptWithBloom {
            receipt:
                Receipt {
                    status,
                    cumulative_gas_used,
                    logs,
                },
            logs_bloom,
        } = rpc;
        ReceiptWithBloom {
            receipt: Receipt {
                status,
                cumulative_gas_used,
                logs: logs.into_iter().map(|it| it.inner).collect(),
            },
            logs_bloom,
        }
    }
}

/// Tracing options for the debug_traceTransaction call.
fn prestate_tracing_options(diff_mode: bool) -> GethDebugTracingOptions {
    GethDebugTracingOptions {
        tracer_config: PreStateConfig {
            diff_mode: Some(diff_mode),
        }
        .into(),
        tracer: Some(GethDebugTracerType::BuiltInTracer(
            GethDebugBuiltInTracerType::PreStateTracer,
        )),
        ..GethDebugTracingOptions::default()
    }
}

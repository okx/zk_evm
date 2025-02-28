use ethereum_types::U256;

pub(crate) fn all_rlp_prover_inputs_reversed(signed_txns: &[Vec<u8>]) -> Vec<U256> {
    let mut inputs = all_rlp_prover_inputs(signed_txns);
    inputs.reverse();
    inputs
}

fn all_rlp_prover_inputs(signed_txns: &[Vec<u8>]) -> Vec<U256> {
    let mut prover_inputs = vec![];
    for txn in signed_txns {
        prover_inputs.push(txn.len().into());
        let mut chunks = txn.chunks_exact(32);
        for bytes in chunks.by_ref() {
            prover_inputs.push(U256::from_big_endian(bytes));
        }
        let mut last_chunk = chunks.remainder().to_vec();
        if !last_chunk.is_empty() {
            last_chunk.extend_from_slice(&vec![0u8; 32 - last_chunk.len()]);
            prover_inputs.push(U256::from_big_endian(&last_chunk));
        }
    }

    prover_inputs
}

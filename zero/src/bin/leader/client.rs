use std::sync::Arc;

use alloy::providers::Provider;
use alloy::rpc::types::{BlockId, BlockNumberOrTag};
use alloy::transports::Transport;
use anyhow::{anyhow, Result};
use tokio::sync::mpsc;
use tracing::info;
use zero::block_interval::{BlockInterval, BlockIntervalStream};
use zero::pre_checks::check_previous_proof_and_checkpoint;
use zero::proof_types::GeneratedBlockProof;
use zero::prover::{self, BlockProverInput, ProverConfig};
use zero::provider::CachedProvider;
use zero::rpc;

use crate::ProofRuntime;

#[derive(Debug)]
pub struct LeaderConfig {
    pub checkpoint_block_number: u64,
    pub previous_proof: Option<GeneratedBlockProof>,
    pub prover_config: ProverConfig,
}

/// The main function for the client.
pub(crate) async fn client_main<ProviderT, TransportT>(
    proof_runtime: Arc<ProofRuntime>,
    cached_provider: Arc<CachedProvider<ProviderT, TransportT>>,
    block_time: u64,
    block_interval: BlockInterval,
    mut leader_config: LeaderConfig,
) -> Result<()>
where
    ProviderT: Provider<TransportT> + 'static,
    TransportT: Transport + Clone,
{
    use futures::StreamExt;

    let test_only = leader_config.prover_config.test_only;

    if !test_only {
        // For actual proof runs, perform a sanity check on the provided inputs.
        check_previous_proof_and_checkpoint(
            leader_config.checkpoint_block_number,
            &leader_config.previous_proof,
            block_interval.get_start_block()?,
        )?;
    }

    // Create a channel for block prover input and use it to send prover input to
    // the proving task. The second element of the tuple is a flag indicating
    // whether the block is the last one in the interval.
    let (block_tx, block_rx) = mpsc::channel::<(BlockProverInput, bool)>(zero::BLOCK_CHANNEL_SIZE);

    // Run proving task
    let proof_runtime_ = proof_runtime.clone();
    let proving_task = tokio::spawn(prover::prove(
        block_rx,
        proof_runtime_,
        leader_config.previous_proof.take(),
        Arc::new(leader_config.prover_config),
    ));

    // Create block interval stream. Could be bounded or unbounded.
    let mut block_interval_stream: BlockIntervalStream = match block_interval {
        block_interval @ BlockInterval::FollowFrom { .. } => {
            block_interval
                .into_unbounded_stream(cached_provider.clone(), block_time)
                .await?
        }
        _ => block_interval.into_bounded_stream()?,
    };

    // Iterate over the block interval, retrieve prover input
    // and send it to the proving task
    while let Some(block_interval_elem) = block_interval_stream.next().await {
        let (block_num, is_last_block) = block_interval_elem?;
        let block_id = BlockId::Number(BlockNumberOrTag::Number(block_num));
        // Get prover input for particular block.
        let block_prover_input = rpc::block_prover_input(
            cached_provider.clone(),
            block_id,
            leader_config.checkpoint_block_number,
        )
        .await?;
        block_tx
            .send((block_prover_input, is_last_block))
            .await
            .map_err(|e| anyhow!("failed to send block prover input through the channel: {e}"))?;
    }

    match proving_task.await {
        Ok(Ok(_)) => {
            info!("Proving task successfully finished");
        }
        Ok(Err(e)) => {
            anyhow::bail!("Proving task finished with error: {e:?}");
        }
        Err(e) => {
            anyhow::bail!("Unable to join proving task, error: {e:?}");
        }
    }

    proof_runtime.light_proof.close().await?;
    proof_runtime.heavy_proof.close().await?;

    if test_only {
        info!("All proof witnesses have been generated successfully.");
    } else {
        info!("All proofs have been generated successfully.");
    }

    Ok(())
}

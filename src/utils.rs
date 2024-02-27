use std::ops::Deref;
use std::sync::Arc;
use std::time::SystemTime;

use color_eyre::eyre::{bail, OptionExt};
use color_eyre::{eyre::eyre, Result};
use lazy_static::lazy_static;
use log::debug;

use starknet::core::types::{
    BlockId, BlockWithTxs, ExecutionResult, MaybePendingBlockWithTxs, StarknetError,
};
use starknet::core::{crypto::compute_hash_on_elements, types::FieldElement};
use starknet::providers::{jsonrpc::HttpTransport, JsonRpcClient, Provider};
use starknet::providers::{MaybeUnknownErrorCode, ProviderError};
use starknet::{
    core::types::MaybePendingTransactionReceipt::{PendingReceipt, Receipt},
    providers::StarknetErrorWithMessage,
};
use tokio::task::JoinSet;

use std::time::Duration;
use sysinfo::{CpuExt, System, SystemExt};

lazy_static! {
    pub static ref SYSINFO: SysInfo = SysInfo::new();
}

/// Cairo string for "STARKNET_CONTRACT_ADDRESS"
const PREFIX_CONTRACT_ADDRESS: FieldElement = FieldElement::from_mont([
    3829237882463328880,
    17289941567720117366,
    8635008616843941496,
    533439743893157637,
]);

/// 2 ** 251 - 256
const ADDR_BOUND: FieldElement = FieldElement::from_mont([
    18446743986131443745,
    160989183,
    18446744073709255680,
    576459263475590224,
]);

// Copied from starknet-rs since it's not public
pub fn compute_contract_address(
    salt: FieldElement,
    class_hash: FieldElement,
    constructor_calldata: &[FieldElement],
) -> FieldElement {
    compute_hash_on_elements(&[
        PREFIX_CONTRACT_ADDRESS,
        FieldElement::ZERO,
        salt,
        class_hash,
        compute_hash_on_elements(constructor_calldata),
    ]) % ADDR_BOUND
}

#[derive(Debug, Clone)]
pub struct SysInfo {
    pub os_name: String,
    pub kernel_version: String,
    pub arch: String,
    pub cpu_count: usize,
    pub cpu_frequency: u64,
    pub cpu_brand: String,
    pub memory: u64,
}

impl SysInfo {
    pub fn new() -> Self {
        let sys = System::new_all();
        let cpu = sys.global_cpu_info();

        Self {
            os_name: sys.long_os_version().unwrap().trim().to_string(),
            kernel_version: sys.kernel_version().unwrap(),
            arch: std::env::consts::ARCH.to_string(),
            cpu_count: sys.cpus().len(),
            cpu_frequency: cpu.frequency(),
            cpu_brand: cpu.brand().to_string(),
            memory: sys.total_memory(),
        }
    }
}

pub fn sysinfo_string() -> String {
    let SysInfo {
        os_name,
        kernel_version,
        arch,
        cpu_count,
        cpu_frequency,
        cpu_brand,
        memory,
    } = SYSINFO.deref();

    let gigabyte_memory = memory / (1024 * 1024 * 1024);

    format!(
        "CPU Count: {cpu_count}\n\
        CPU Model: {cpu_brand}\n\
        CPU Speed (MHz): {cpu_frequency}\n\
        Total Memory: {gigabyte_memory} GB\n\
        Platform: {os_name}\n\
        Release: {kernel_version}\n\
        Architecture: {arch}",
    )
}

impl Default for SysInfo {
    fn default() -> Self {
        Self::new()
    }
}

const WAIT_FOR_TX_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn wait_for_tx(
    provider: &JsonRpcClient<HttpTransport>,
    tx_hash: FieldElement,
    check_interval: Duration,
) -> Result<()> {
    let start = SystemTime::now();

    loop {
        if start.elapsed().unwrap() >= WAIT_FOR_TX_TIMEOUT {
            return Err(eyre!(
                "Timeout while waiting for transaction {tx_hash:#064x}"
            ));
        }

        match provider.get_transaction_receipt(tx_hash).await {
            Ok(Receipt(receipt)) => {
                // Logic copied from starkli and the following comment too
                // tWith JSON-RPC, once we get a receipt, the transaction must have been confirmed.
                // Rejected transactions simply aren't available. This needs to be changed once we
                // implement the sequencer fallback.

                match receipt.execution_result() {
                    ExecutionResult::Succeeded => {
                        return Ok(());
                    }
                    ExecutionResult::Reverted { reason } => {
                        return Err(eyre!(format!(
                            "Transaction {tx_hash:#064x} has been rejected/reverted: {reason}"
                        )));
                    }
                }
            }
            Ok(PendingReceipt(pending)) => {
                if let ExecutionResult::Reverted { reason } = pending.execution_result() {
                    return Err(eyre!(format!(
                        "Transaction {tx_hash:#064x} has been rejected/reverted: {reason}"
                    )));
                }
                debug!("Waiting for transaction {tx_hash:#064x} to be accepted");
                tokio::time::sleep(check_interval).await;
            }
            Err(ProviderError::StarknetError(StarknetErrorWithMessage {
                code: MaybeUnknownErrorCode::Known(StarknetError::TransactionHashNotFound),
                ..
            })) => {
                debug!("Waiting for transaction {tx_hash:#064x} to show up");
                tokio::time::sleep(check_interval).await;
            }
            Err(err) => {
                return Err(eyre!(err).wrap_err(format!(
                    "Error while waiting for transaction {tx_hash:#064x}"
                )))
            }
        }
    }
}

/// Get a list of blocks with transaction information from
/// `start_block` to `end_block` (including both)
/// This is meant to be used to calculate multiple metrics such as TPS and UOPS
/// without hitting the StarkNet RPC multiple times
pub async fn get_blocks_with_txs(
    starknet_rpc: &Arc<JsonRpcClient<HttpTransport>>,
    block_range: impl Iterator<Item = u64>,
) -> Result<Vec<BlockWithTxs>> {
    const MAX_CONCURRENT: usize = 50;

    // A collection of spawned tokio tasks
    let mut join_set = JoinSet::new();

    let mut results = Vec::with_capacity(block_range.size_hint().0);

    for block_number in block_range {
        // Make sure we don't hit dev server with too many requests
        while join_set.len() >= MAX_CONCURRENT {
            let next = join_set
                .join_next()
                .await
                .ok_or_eyre("JoinSet should have items")???;

            results.push(match_result(next)?);
        }

        let starknet_rpc = starknet_rpc.clone();

        join_set.spawn(async move {
            starknet_rpc
                .get_block_with_txs(BlockId::Number(block_number))
                .await
        });
    }

    // Process the rest
    while let Some(next) = join_set.join_next().await {
        results.push(match_result(next??)?)
    }

    fn match_result(maybe_block: MaybePendingBlockWithTxs) -> Result<BlockWithTxs> {
        match maybe_block {
            MaybePendingBlockWithTxs::Block(block) => Ok(block),
            MaybePendingBlockWithTxs::PendingBlock(_) => bail!("Blocks should not be pending!"),
        }
    }

    Ok(results)
}

/// Sanitize a string to be used as a filename by removing/replacing illegal chars
pub fn sanitize_filename(input: &str) -> String {
    // Define a set of characters to replace or remove
    let invalid_chars: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|', ' '];

    // Replace invalid characters with underscores and remove control characters
    let sanitized = input
        .to_lowercase()
        .chars()
        .map(|c| {
            if invalid_chars.contains(&c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect::<String>();

    // Truncate the string to a reasonable length for file names
    let max_length = 255; // Maximum file name length for many file systems
    let truncated = if sanitized.len() > max_length {
        &sanitized[..max_length]
    } else {
        &sanitized
    };

    truncated.to_string()
}

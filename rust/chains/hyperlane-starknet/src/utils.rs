use std::future::Future;

use cainome::cairo_serde::CairoSerde;
use hyperlane_core::{ChainResult, HyperlaneMessage, ModuleType};
use starknet::{
    accounts::SingleOwnerAccount,
    core::{
        chain_id::{MAINNET, SEPOLIA},
        types::{EmittedEvent, FieldElement, MaybePendingTransactionReceipt},
    },
    providers::{jsonrpc::HttpTransport, AnyProvider, JsonRpcClient, Provider, ProviderError},
    signers::LocalWallet,
};
use url::Url;

use crate::{
    contracts::{interchain_security_module::ModuleType as StarknetModuleType, mailbox::Message},
    HyperlaneStarknetError,
};

/// Polls a function until it returns true or the max poll count is exceeded.
pub async fn assert_poll<F, Fut>(f: F, polling_time_ms: u64, max_poll_count: u32)
where
    F: Fn() -> Fut,
    Fut: Future<Output = bool>,
{
    for _poll_count in 0..max_poll_count {
        if f().await {
            return; // The provided function returned true, exit safely.
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(polling_time_ms)).await;
    }

    panic!("Max poll count exceeded.");
}

type TransactionReceiptResult = Result<MaybePendingTransactionReceipt, ProviderError>;

/// Polls the rpc client until the transaction receipt is available.
pub async fn get_transaction_receipt(
    rpc: &AnyProvider,
    transaction_hash: FieldElement,
) -> TransactionReceiptResult {
    // there is a delay between the transaction being available at the client
    // and the sealing of the block, hence sleeping for 100ms
    assert_poll(
        || async { rpc.get_transaction_receipt(transaction_hash).await.is_ok() },
        100,
        20,
    )
    .await;

    rpc.get_transaction_receipt(transaction_hash).await
}

const KATANA: FieldElement = FieldElement::from_mont([
    18444096267036800993,
    18446744073709551615,
    18446744073709551615,
    531448038866662896,
]);

/// Returns the starknet chain id from the hyperlane domain id.
pub fn get_chain_id_from_domain_id(domain_id: u32) -> FieldElement {
    match domain_id {
        23448591 => SEPOLIA,
        23448592 => MAINNET,
        23448593 => KATANA,
        23448594 => KATANA,
        _ => panic!("Unsupported domain id"),
    }
}

/// Creates a single owner account for a given signer and account address.
///
/// # Arguments
///
/// * `rpc_url` - The rpc url of the chain.
/// * `signer` - The signer of the account.
/// * `account_address` - The address of the account.
/// * `is_legacy` - Whether the account is legacy (Cairo 0) or not.
/// * `domain_id` - The hyperlane domain id of the chain.
pub fn build_single_owner_account(
    rpc_url: &Url,
    signer: LocalWallet,
    account_address: &FieldElement,
    is_legacy: bool,
    domain_id: u32,
) -> SingleOwnerAccount<AnyProvider, LocalWallet> {
    let rpc_client =
        AnyProvider::JsonRpcHttp(JsonRpcClient::new(HttpTransport::new(rpc_url.clone())));

    let execution_encoding = if is_legacy {
        starknet::accounts::ExecutionEncoding::Legacy
    } else {
        starknet::accounts::ExecutionEncoding::New
    };

    let chain_id = get_chain_id_from_domain_id(domain_id);

    SingleOwnerAccount::new(
        rpc_client,
        signer,
        *account_address,
        chain_id,
        execution_encoding,
    )
}

/// Converts a starknet module type to a hyperlane module type.
pub fn to_hpl_module_type(module_type: StarknetModuleType) -> ModuleType {
    match module_type {
        StarknetModuleType::UNUSED(_) => ModuleType::Unused,
        StarknetModuleType::ROUTING(_) => ModuleType::Routing,
        StarknetModuleType::AGGREGATION(_) => ModuleType::Aggregation,
        StarknetModuleType::LEGACY_MULTISIG(_) => ModuleType::LegacyMultisig,
        StarknetModuleType::MERKLE_ROOT_MULTISIG(_) => ModuleType::MerkleRootMultisig,
        StarknetModuleType::MESSAGE_ID_MULTISIG(_) => ModuleType::MessageIdMultisig,
        StarknetModuleType::NULL => ModuleType::Null,
        StarknetModuleType::CCIP_READ(_) => ModuleType::CcipRead,
    }
}

/// Parses a hyperlane message from a starknet emitted event.
/// We use the CairoSerde trait to deserialize the message.
pub fn try_parse_hyperlane_message_from_event(
    event: &EmittedEvent,
) -> ChainResult<HyperlaneMessage> {
    let sender = event.data[0].into();
    let destination = event.data[1]
        .try_into()
        .map_err(Into::<HyperlaneStarknetError>::into)?;
    let recipient = event.data[2].into();
    let message =
        Message::cairo_deserialize(&event.data, 3).map_err(Into::<HyperlaneStarknetError>::into)?;

    Ok(HyperlaneMessage {
        version: message.version,
        nonce: message.nonce,
        origin: message.origin,
        sender,
        destination,
        recipient,
        body: u128_vec_to_u8_vec(message.body.data),
    })
}

/// Converts a Vec<u128> to a Vec<u8>.
fn u128_vec_to_u8_vec(input: Vec<u128>) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len() * 16);
    for value in input {
        output.extend_from_slice(&value.to_be_bytes());
    }
    output
}
use async_trait::async_trait;
use eyre::{bail, Result};
use paste::paste;
use tracing::{info, instrument, trace};

use hyperlane_core::{
    GasPaymentKey, HyperlaneDomain, HyperlaneLogStore, HyperlaneMessage,
    HyperlaneSequenceAwareIndexerStoreReader, HyperlaneWatermarkedLogStore, Indexed,
    InterchainGasExpenditure, InterchainGasPayment, InterchainGasPaymentMeta, LogMeta,
    MerkleTreeInsertion, PendingOperationStatus, H256,
};

use super::{
    storage_types::{InterchainGasExpenditureData, InterchainGasPaymentData},
    DbError, TypedDB, DB,
};

// these keys MUST not be given multiple uses in case multiple agents are
// started with the same database and domain.

const MESSAGE_ID: &str = "message_id_";
const MESSAGE_DISPATCHED_BLOCK_NUMBER: &str = "message_dispatched_block_number_";
const MESSAGE: &str = "message_";
const NONCE_PROCESSED: &str = "nonce_processed_";
const GAS_PAYMENT_BY_SEQUENCE: &str = "gas_payment_by_sequence_";
const HIGHEST_SEEN_MESSAGE_NONCE: &str = "highest_seen_message_nonce_";
const GAS_PAYMENT_FOR_MESSAGE_ID: &str = "gas_payment_sequence_for_message_id_v2_";
const GAS_PAYMENT_META_PROCESSED: &str = "gas_payment_meta_processed_v3_";
const GAS_EXPENDITURE_FOR_MESSAGE_ID: &str = "gas_expenditure_for_message_id_v2_";
const STATUS_BY_MESSAGE_ID: &str = "status_by_message_id_";
const PENDING_MESSAGE_RETRY_COUNT_FOR_MESSAGE_ID: &str =
    "pending_message_retry_count_for_message_id_";
const MERKLE_TREE_INSERTION: &str = "merkle_tree_insertion_";
const MERKLE_LEAF_INDEX_BY_MESSAGE_ID: &str = "merkle_leaf_index_by_message_id_";
const MERKLE_TREE_INSERTION_BLOCK_NUMBER_BY_LEAF_INDEX: &str =
    "merkle_tree_insertion_block_number_by_leaf_index_";
const LATEST_INDEXED_GAS_PAYMENT_BLOCK: &str = "latest_indexed_gas_payment_block";

/// Rocks DB result type
pub type DbResult<T> = std::result::Result<T, DbError>;

/// DB handle for storing data tied to a specific Mailbox.
#[derive(Debug, Clone)]
pub struct HyperlaneRocksDB(HyperlaneDomain, TypedDB);

impl std::ops::Deref for HyperlaneRocksDB {
    type Target = TypedDB;

    fn deref(&self) -> &Self::Target {
        &self.1
    }
}

impl AsRef<TypedDB> for HyperlaneRocksDB {
    fn as_ref(&self) -> &TypedDB {
        &self.1
    }
}

impl AsRef<DB> for HyperlaneRocksDB {
    fn as_ref(&self) -> &DB {
        self.1.as_ref()
    }
}

impl HyperlaneRocksDB {
    /// Instantiated new `HyperlaneRocksDB`
    pub fn new(domain: &HyperlaneDomain, db: DB) -> Self {
        Self(domain.clone(), TypedDB::new(domain, db))
    }

    /// Get the domain this database is scoped to
    pub fn domain(&self) -> &HyperlaneDomain {
        &self.0
    }

    /// Store a raw committed message
    ///
    /// Keys --> Values:
    /// - `nonce` --> `id`
    /// - `id` --> `message`
    /// - `nonce` --> `dispatched block number`
    pub fn store_message(
        &self,
        message: &HyperlaneMessage,
        dispatched_block_number: u64,
    ) -> DbResult<bool> {
        if let Ok(Some(_)) = self.retrieve_message_id_by_nonce(&message.nonce) {
            trace!(msg=?message, "Message already stored in db");
            return Ok(false);
        }

        let id = message.id();
        info!(msg=?message,  "Storing new message in db",);

        // - `id` --> `message`
        self.store_message_by_id(&id, message)?;
        // - `nonce` --> `id`
        self.store_message_id_by_nonce(&message.nonce, &id)?;
        // Update the max seen nonce to allow forward-backward iteration in the processor
        self.try_update_max_seen_message_nonce(message.nonce)?;
        // - `nonce` --> `dispatched block number`
        self.store_dispatched_block_number_by_nonce(&message.nonce, &dispatched_block_number)?;
        Ok(true)
    }

    /// Retrieve a message by its nonce
    pub fn retrieve_message_by_nonce(&self, nonce: u32) -> DbResult<Option<HyperlaneMessage>> {
        let id = self.retrieve_message_id_by_nonce(&nonce)?;
        match id {
            None => Ok(None),
            Some(id) => self.retrieve_message_by_id(&id),
        }
    }

    /// Update the nonce of the highest processed message we're aware of
    pub fn try_update_max_seen_message_nonce(&self, nonce: u32) -> DbResult<()> {
        let current_max = self
            .retrieve_highest_seen_message_nonce()?
            .unwrap_or_default();
        if nonce >= current_max {
            self.store_highest_seen_message_nonce_number(&Default::default(), &nonce)?;
        }
        Ok(())
    }

    /// Retrieve the nonce of the highest processed message we're aware of
    pub fn retrieve_highest_seen_message_nonce(&self) -> DbResult<Option<u32>> {
        self.retrieve_highest_seen_message_nonce_number(&Default::default())
    }

    /// If the provided gas payment, identified by its metadata, has not been
    /// processed, processes the gas payment and records it as processed.
    /// Returns whether the gas payment was processed for the first time.
    pub fn process_indexed_gas_payment(
        &self,
        indexed_payment: Indexed<InterchainGasPayment>,
        log_meta: &LogMeta,
    ) -> DbResult<bool> {
        let payment = *(indexed_payment.inner());
        let gas_processing_successful = self.process_gas_payment(payment, log_meta)?;

        // only store the payment and return early if there's no sequence
        let Some(gas_payment_sequence) = indexed_payment.sequence else {
            return Ok(gas_processing_successful);
        };
        // otherwise store the indexing decorator as well
        if let Ok(Some(_)) = self.retrieve_gas_payment_by_sequence(&gas_payment_sequence) {
            trace!(
                ?indexed_payment,
                ?log_meta,
                "Attempted to process an already-processed indexed gas payment"
            );
            // Return false to indicate the gas payment was already processed
            return Ok(false);
        }

        self.store_gas_payment_by_sequence(&gas_payment_sequence, indexed_payment.inner())?;
        self.store_gas_payment_block_by_sequence(&gas_payment_sequence, &log_meta.block_number)?;

        Ok(gas_processing_successful)
    }

    /// If the provided gas payment, identified by its metadata, has not been
    /// processed, processes the gas payment and records it as processed.
    /// Returns whether the gas payment was processed for the first time.
    pub fn process_gas_payment(
        &self,
        payment: InterchainGasPayment,
        log_meta: &LogMeta,
    ) -> DbResult<bool> {
        let payment_meta = log_meta.into();
        // If the gas payment has already been processed, do nothing
        if self
            .retrieve_processed_by_gas_payment_meta(&payment_meta)?
            .unwrap_or(false)
        {
            trace!(
                ?payment,
                ?log_meta,
                "Attempted to process an already-processed gas payment"
            );
            // Return false to indicate the gas payment was already processed
            return Ok(false);
        }
        // Set the gas payment as processed
        self.store_processed_by_gas_payment_meta(&payment_meta, &true)?;

        // Update the total gas payment for the message to include the payment
        self.update_gas_payment_by_gas_payment_key(payment)?;

        // Return true to indicate the gas payment was processed for the first time
        Ok(true)
    }

    /// Store the merkle tree insertion event, and also store a mapping from message_id to leaf_index
    pub fn process_tree_insertion(
        &self,
        insertion: &MerkleTreeInsertion,
        insertion_block_number: u64,
    ) -> DbResult<bool> {
        if let Ok(Some(_)) = self.retrieve_merkle_tree_insertion_by_leaf_index(&insertion.index()) {
            info!(insertion=?insertion, "Tree insertion already stored in db");
            return Ok(false);
        }

        // even if double insertions are ok, store the leaf by `leaf_index` (guaranteed to be unique)
        // rather than by `message_id` (not guaranteed to be recurring), so that leaves can be retrieved
        // based on insertion order.
        self.store_merkle_tree_insertion_by_leaf_index(&insertion.index(), insertion)?;

        self.store_merkle_leaf_index_by_message_id(&insertion.message_id(), &insertion.index())?;

        self.store_merkle_tree_insertion_block_number_by_leaf_index(
            &insertion.index(),
            &insertion_block_number,
        )?;
        // Return true to indicate the tree insertion was processed
        Ok(true)
    }

    /// Processes the gas expenditure and store the total expenditure for the
    /// message.
    pub fn process_gas_expenditure(&self, expenditure: InterchainGasExpenditure) -> DbResult<()> {
        // Update the total gas expenditure for the message to include the payment
        self.update_gas_expenditure_by_message_id(expenditure)
    }

    /// Update the total gas payment for a message to include gas_payment
    fn update_gas_payment_by_gas_payment_key(&self, event: InterchainGasPayment) -> DbResult<()> {
        let gas_payment_key = event.into();
        let existing_payment =
            match self.retrieve_gas_payment_by_gas_payment_key(gas_payment_key)? {
                Some(payment) => payment,
                None => InterchainGasPayment::from_gas_payment_key(gas_payment_key),
            };
        let total = existing_payment + event;

        info!(?event, new_total_gas_payment=?total, "Storing gas payment");
        self.store_interchain_gas_payment_data_by_gas_payment_key(&gas_payment_key, &total.into())?;

        Ok(())
    }

    /// Update the total gas spent for a message
    fn update_gas_expenditure_by_message_id(
        &self,
        event: InterchainGasExpenditure,
    ) -> DbResult<()> {
        let existing_expenditure = self.retrieve_gas_expenditure_by_message_id(event.message_id)?;
        let total = existing_expenditure + event;

        info!(?event, new_total_gas_expenditure=?total, "Storing gas expenditure");
        self.store_interchain_gas_expenditure_data_by_message_id(
            &total.message_id,
            &InterchainGasExpenditureData {
                tokens_used: total.tokens_used,
                gas_used: total.gas_used,
            },
        )?;
        Ok(())
    }

    /// Retrieve the total gas payment for a message
    pub fn retrieve_gas_payment_by_gas_payment_key(
        &self,
        gas_payment_key: GasPaymentKey,
    ) -> DbResult<Option<InterchainGasPayment>> {
        Ok(self
            .retrieve_interchain_gas_payment_data_by_gas_payment_key(&gas_payment_key)?
            .map(|payment| {
                payment.complete(gas_payment_key.message_id, gas_payment_key.destination)
            }))
    }

    /// Retrieve the total gas payment for a message
    pub fn retrieve_gas_expenditure_by_message_id(
        &self,
        message_id: H256,
    ) -> DbResult<InterchainGasExpenditure> {
        Ok(self
            .retrieve_interchain_gas_expenditure_data_by_message_id(&message_id)?
            .unwrap_or_default()
            .complete(message_id))
    }
}

#[async_trait]
impl HyperlaneLogStore<HyperlaneMessage> for HyperlaneRocksDB {
    /// Store a list of dispatched messages and their associated metadata.
    #[instrument(skip_all)]
    async fn store_logs(&self, messages: &[(Indexed<HyperlaneMessage>, LogMeta)]) -> Result<u32> {
        let mut stored = 0;
        for (message, meta) in messages {
            let stored_message = self.store_message(message.inner(), meta.block_number)?;
            if stored_message {
                stored += 1;
            }
        }
        if stored > 0 {
            info!(messages = stored, "Wrote new messages to database");
        }
        Ok(stored)
    }
}

async fn store_and_count_new<T: Copy>(
    store: &HyperlaneRocksDB,
    logs: &[(T, LogMeta)],
    log_type: &str,
    process: impl Fn(&HyperlaneRocksDB, T, &LogMeta) -> DbResult<bool>,
) -> Result<u32> {
    let mut new_logs = 0;
    for (log, meta) in logs {
        if process(store, *log, meta)? {
            new_logs += 1;
        }
    }
    if new_logs > 0 {
        info!(new_logs, log_type, "Wrote new logs to database");
    }
    Ok(new_logs)
}

#[async_trait]
impl HyperlaneLogStore<InterchainGasPayment> for HyperlaneRocksDB {
    /// Store a list of interchain gas payments and their associated metadata.
    #[instrument(skip_all)]
    async fn store_logs(
        &self,
        payments: &[(Indexed<InterchainGasPayment>, LogMeta)],
    ) -> Result<u32> {
        store_and_count_new(
            self,
            payments,
            "gas payments",
            HyperlaneRocksDB::process_indexed_gas_payment,
        )
        .await
    }
}

#[async_trait]
impl HyperlaneLogStore<MerkleTreeInsertion> for HyperlaneRocksDB {
    /// Store every tree insertion event
    #[instrument(skip_all)]
    async fn store_logs(&self, leaves: &[(Indexed<MerkleTreeInsertion>, LogMeta)]) -> Result<u32> {
        let mut insertions = 0;
        for (insertion, meta) in leaves {
            if self.process_tree_insertion(insertion.inner(), meta.block_number)? {
                insertions += 1;
            }
        }
        Ok(insertions)
    }
}

#[async_trait]
impl HyperlaneSequenceAwareIndexerStoreReader<HyperlaneMessage> for HyperlaneRocksDB {
    /// Gets data by its sequence.
    async fn retrieve_by_sequence(&self, sequence: u32) -> Result<Option<HyperlaneMessage>> {
        let message = self.retrieve_message_by_nonce(sequence)?;
        Ok(message)
    }

    /// Gets the block number at which the log occurred.
    async fn retrieve_log_block_number_by_sequence(&self, sequence: u32) -> Result<Option<u64>> {
        let number = self.retrieve_dispatched_block_number_by_nonce(&sequence)?;
        Ok(number)
    }
}

#[async_trait]
impl HyperlaneSequenceAwareIndexerStoreReader<MerkleTreeInsertion> for HyperlaneRocksDB {
    /// Gets data by its sequence.
    async fn retrieve_by_sequence(&self, sequence: u32) -> Result<Option<MerkleTreeInsertion>> {
        let insertion = self.retrieve_merkle_tree_insertion_by_leaf_index(&sequence)?;
        Ok(insertion)
    }

    /// Gets the block number at which the log occurred.
    async fn retrieve_log_block_number_by_sequence(&self, sequence: u32) -> Result<Option<u64>> {
        let number = self.retrieve_merkle_tree_insertion_block_number_by_leaf_index(&sequence)?;
        Ok(number)
    }
}

// TODO: replace this blanket implementation to be able to do sequence-aware indexing
#[async_trait]
impl HyperlaneSequenceAwareIndexerStoreReader<InterchainGasPayment> for HyperlaneRocksDB {
    /// Gets data by its sequence.
    async fn retrieve_by_sequence(&self, sequence: u32) -> Result<Option<InterchainGasPayment>> {
        Ok(self.retrieve_gas_payment_by_sequence(&sequence)?)
    }

    /// Gets the block number at which the log occurred.
    async fn retrieve_log_block_number_by_sequence(&self, sequence: u32) -> Result<Option<u64>> {
        Ok(self.retrieve_gas_payment_block_by_sequence(&sequence)?)
    }
}

#[async_trait]
impl HyperlaneWatermarkedLogStore<InterchainGasPayment> for HyperlaneRocksDB {
    /// Gets the block number high watermark
    async fn retrieve_high_watermark(&self) -> Result<Option<u32>> {
        let watermark = self.retrieve_decodable("", LATEST_INDEXED_GAS_PAYMENT_BLOCK)?;
        Ok(watermark)
    }

    /// Stores the block number high watermark
    async fn store_high_watermark(&self, block_number: u32) -> Result<()> {
        let result = self.store_encodable("", LATEST_INDEXED_GAS_PAYMENT_BLOCK, &block_number)?;
        Ok(result)
    }
}

// Keep this implementation for type compatibility with the `contract_syncs` sync builder
#[async_trait]
impl HyperlaneWatermarkedLogStore<HyperlaneMessage> for HyperlaneRocksDB {
    /// Gets the block number high watermark
    async fn retrieve_high_watermark(&self) -> Result<Option<u32>> {
        bail!("Not implemented")
    }

    /// Stores the block number high watermark
    async fn store_high_watermark(&self, _block_number: u32) -> Result<()> {
        bail!("Not implemented")
    }
}

// Keep this implementation for type compatibility with the `contract_syncs` sync builder
#[async_trait]
impl HyperlaneWatermarkedLogStore<MerkleTreeInsertion> for HyperlaneRocksDB {
    /// Gets the block number high watermark
    async fn retrieve_high_watermark(&self) -> Result<Option<u32>> {
        bail!("Not implemented")
    }

    /// Stores the block number high watermark
    async fn store_high_watermark(&self, _block_number: u32) -> Result<()> {
        bail!("Not implemented")
    }
}

/// Database interface required for processing messages
pub trait ProcessMessage: Send + Sync {
    /// Retrieve the nonce of the highest processed message we're aware of
    fn retrieve_highest_seen_message_nonce(&self) -> DbResult<Option<u32>>;

    /// Retrieve a message by its nonce
    fn retrieve_message_by_nonce(&self, nonce: u32) -> DbResult<Option<HyperlaneMessage>>;

    /// Retrieve whether a message has been processed
    fn retrieve_processed_by_nonce(&self, nonce: u32) -> DbResult<Option<bool>>;

    /// Get the origin domain of the database
    fn domain(&self) -> &HyperlaneDomain;
}

impl ProcessMessage for HyperlaneRocksDB {
    fn retrieve_highest_seen_message_nonce(&self) -> DbResult<Option<u32>> {
        self.retrieve_highest_seen_message_nonce()
    }

    fn retrieve_message_by_nonce(&self, nonce: u32) -> DbResult<Option<HyperlaneMessage>> {
        self.retrieve_message_by_nonce(nonce)
    }

    fn retrieve_processed_by_nonce(&self, nonce: u32) -> DbResult<Option<bool>> {
        self.retrieve_processed_by_nonce(&nonce)
    }

    fn domain(&self) -> &HyperlaneDomain {
        self.domain()
    }
}

/// Generate a call to ChainSetup for the given builder
macro_rules! make_store_and_retrieve {
    ($vis:vis, $name_suffix:ident, $key_prefix: ident, $key_ty:ty, $val_ty:ty$(,)?) => {
        impl HyperlaneRocksDB {
            paste! {
                /// Stores a key value pair in the DB
                $vis fn [<store_ $name_suffix>] (
                    &self,
                    key: &$key_ty,
                    val: &$val_ty,
                ) -> DbResult<()> {
                    self.store_keyed_encodable($key_prefix, key, val)
                }

                /// Retrieves a key value pair from the DB
                $vis fn [<retrieve_ $name_suffix>] (
                    &self,
                    key: &$key_ty,
                ) -> DbResult<Option<$val_ty>> {
                    self.retrieve_keyed_decodable($key_prefix, key)
                }
            }
        }
    };
}

make_store_and_retrieve!(pub, message_id_by_nonce, MESSAGE_ID, u32, H256);
make_store_and_retrieve!(pub(self), message_by_id, MESSAGE, H256, HyperlaneMessage);
make_store_and_retrieve!(pub(self), dispatched_block_number_by_nonce, MESSAGE_DISPATCHED_BLOCK_NUMBER, u32, u64);
make_store_and_retrieve!(pub, processed_by_nonce, NONCE_PROCESSED, u32, bool);
make_store_and_retrieve!(pub(self), processed_by_gas_payment_meta, GAS_PAYMENT_META_PROCESSED, InterchainGasPaymentMeta, bool);
make_store_and_retrieve!(pub(self), interchain_gas_expenditure_data_by_message_id, GAS_EXPENDITURE_FOR_MESSAGE_ID, H256, InterchainGasExpenditureData);
make_store_and_retrieve!(
    pub,
    status_by_message_id,
    STATUS_BY_MESSAGE_ID,
    H256,
    PendingOperationStatus
);
make_store_and_retrieve!(pub(self), interchain_gas_payment_data_by_gas_payment_key, GAS_PAYMENT_FOR_MESSAGE_ID, GasPaymentKey, InterchainGasPaymentData);
make_store_and_retrieve!(pub(self), gas_payment_by_sequence, GAS_PAYMENT_BY_SEQUENCE, u32, InterchainGasPayment);
make_store_and_retrieve!(pub(self), gas_payment_block_by_sequence, GAS_PAYMENT_BY_SEQUENCE, u32, u64);
make_store_and_retrieve!(
    pub,
    pending_message_retry_count_by_message_id,
    PENDING_MESSAGE_RETRY_COUNT_FOR_MESSAGE_ID,
    H256,
    u32
);
make_store_and_retrieve!(
    pub,
    merkle_tree_insertion_by_leaf_index,
    MERKLE_TREE_INSERTION,
    u32,
    MerkleTreeInsertion
);
make_store_and_retrieve!(
    pub,
    merkle_leaf_index_by_message_id,
    MERKLE_LEAF_INDEX_BY_MESSAGE_ID,
    H256,
    u32
);
make_store_and_retrieve!(
    pub,
    merkle_tree_insertion_block_number_by_leaf_index,
    MERKLE_TREE_INSERTION_BLOCK_NUMBER_BY_LEAF_INDEX,
    u32,
    u64
);
// There's no unit struct Encode/Decode impl, so just use `bool`, have visibility be private (by omitting the first argument), and wrap
// with a function that always uses the `Default::default()` key
make_store_and_retrieve!(, highest_seen_message_nonce_number, HIGHEST_SEEN_MESSAGE_NONCE, bool, u32);

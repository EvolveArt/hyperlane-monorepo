use std::{collections::HashMap, fmt::Debug, hash::Hash, sync::Arc};

use eyre::{eyre, Context, Result};
use futures_util::future::try_join_all;
use hyperlane_core::{
    HyperlaneChain, HyperlaneDomain, HyperlaneLogStore, HyperlaneProvider, Indexer,
    InterchainGasPaymaster, Mailbox, MerkleTreeHook, MultisigIsm, ValidatorAnnounce, H256,
};

use crate::{
    settings::{chains::ChainConf, trace::TracingConfig},
    ContractSync, ContractSyncMetrics, CoreMetrics, HyperlaneAgentCore, Server,
};

use super::TryFromWithMetrics;

/// Settings. Usually this should be treated as a base config and used as
/// follows:
///
/// ```ignore
/// use hyperlane_base::*;
/// use serde::Deserialize;
///
/// pub struct OtherSettings { /* anything */ };
///
/// #[derive(Debug, Deserialize)]
/// pub struct MySettings {
///     #[serde(flatten)]
///     base_settings: Settings,
///     #[serde(flatten)]
///     other_settings: (),
/// }
///
/// // Make sure to define MySettings::new()
/// impl MySettings {
///     fn new() -> Self {
///         unimplemented!()
///     }
/// }
/// ```
#[derive(Debug, Default)]
pub struct Settings {
    /// Configuration for contracts on each chain
    pub chains: HashMap<String, ChainConf>,
    /// Port to listen for prometheus scrape requests
    pub metrics_port: u16,
    /// The tracing configuration
    pub tracing: TracingConfig,
}

impl Settings {
    /// Generate an agent core
    pub fn build_hyperlane_core(&self, metrics: Arc<CoreMetrics>) -> HyperlaneAgentCore {
        HyperlaneAgentCore {
            metrics,
            settings: self.clone(),
        }
    }

    /// Try to get a MultisigIsm
    pub async fn build_multisig_ism(
        &self,
        domain: &HyperlaneDomain,
        address: H256,
        metrics: &CoreMetrics,
    ) -> Result<Box<dyn MultisigIsm>> {
        let setup = self
            .chain_setup(domain)
            .with_context(|| format!("Building multisig ism for {domain}"))?;
        setup.build_multisig_ism(address, metrics).await
    }

    /// Try to get the chain configuration for the given domain.
    pub fn chain_setup(&self, domain: &HyperlaneDomain) -> Result<&ChainConf> {
        self.chains
            .get(domain.name())
            .ok_or_else(|| eyre!("No chain setup found for {domain}"))
    }

    /// Try to get the domain for a given chain by name.
    pub fn lookup_domain(&self, chain_name: &str) -> Result<HyperlaneDomain> {
        self.chains
            .get(chain_name)
            .ok_or_else(|| eyre!("No chain setup found for {chain_name}"))
            .map(|c| c.domain.clone())
    }

    /// Create the core metrics from the settings given the name of the agent.
    pub fn metrics(&self, name: &str) -> Result<Arc<CoreMetrics>> {
        Ok(Arc::new(CoreMetrics::new(
            name,
            self.metrics_port,
            prometheus::Registry::new(),
        )?))
    }

    /// Create the server from the settings given the name of the agent.
    pub fn server(&self, core_metrics: Arc<CoreMetrics>) -> Result<Arc<Server>> {
        Ok(Arc::new(Server::new(self.metrics_port, core_metrics)))
    }

    /// Private to preserve linearity of AgentCore::from_settings -- creating an
    /// agent consumes the settings.
    fn clone(&self) -> Self {
        Self {
            chains: self.chains.clone(),
            metrics_port: self.metrics_port,
            tracing: self.tracing.clone(),
        }
    }
}

/// Generate a call to ChainSetup for the given builder
macro_rules! build_contract_fns {
    ($singular:ident, $plural:ident -> $ret:ty) => {
        /// Delegates building to ChainSetup
        pub async fn $singular(
            &self,
            domain: &HyperlaneDomain,
            metrics: &CoreMetrics,
        ) -> eyre::Result<Box<$ret>> {
            let setup = self.chain_setup(domain)?;
            setup.$singular(metrics).await
        }

        /// Builds a contract for each domain
        pub async fn $plural(
            &self,
            domains: impl Iterator<Item = &HyperlaneDomain>,
            metrics: &CoreMetrics,
        ) -> Result<HashMap<HyperlaneDomain, Arc<$ret>>> {
            try_join_all(domains.map(|d| self.$singular(d, metrics)))
                .await?
                .into_iter()
                .map(|i| Ok((i.domain().clone(), Arc::from(i))))
                .collect()
        }
    };
}

impl Settings {
    build_contract_fns!(build_interchain_gas_paymaster, build_interchain_gas_paymasters -> dyn InterchainGasPaymaster);
    build_contract_fns!(build_mailbox, build_mailboxes -> dyn Mailbox);
    build_contract_fns!(build_merkle_tree_hook, build_merkle_tree_hooks -> dyn MerkleTreeHook);
    build_contract_fns!(build_validator_announce, build_validator_announces -> dyn ValidatorAnnounce);
    build_contract_fns!(build_provider, build_providers -> dyn HyperlaneProvider);

    /// Build a contract sync for type `T` using indexer `I` and log store `D`
    pub async fn build_contract_sync<T, D, I>(
        &self,
        domain: &HyperlaneDomain,
        metrics: &CoreMetrics,
        sync_metrics: &ContractSyncMetrics,
        db: D,
    ) -> eyre::Result<Arc<ContractSync<T, D, I>>>
    where
        D: HyperlaneLogStore<T> + Clone + 'static,
        I: TryFromWithMetrics<ChainConf> + Indexer<T>,
    {
        let setup = self.chain_setup(domain)?;
        let indexer = I::try_from_with_metrics(setup, metrics).await?;
        let sync = ContractSync::new(domain.clone(), db.clone(), indexer, sync_metrics.clone());

        Ok(Arc::new(sync))
    }

    /// Build multiple contract syncs
    pub async fn build_contract_syncs<T, D, I>(
        &self,
        domains: impl Iterator<Item = &HyperlaneDomain>,
        metrics: &CoreMetrics,
        sync_metrics: &ContractSyncMetrics,
        dbs: HashMap<HyperlaneDomain, D>,
    ) -> Result<HashMap<HyperlaneDomain, Arc<ContractSync<T, D, I>>>>
    where
        T: Debug + Send + Sync + Clone + Eq + Hash + 'static,
        D: HyperlaneLogStore<T> + Clone + 'static,
        I: TryFromWithMetrics<ChainConf> + Indexer<T> + 'static,
    {
        try_join_all(domains.map(|d| {
            self.build_contract_sync::<T, D, I>(
                d,
                metrics,
                sync_metrics,
                dbs.get(d).unwrap().clone(),
            )
        }))
        .await?
        .into_iter()
        .map(|i| Ok((i.domain().clone(), i)))
        .collect()
    }
}

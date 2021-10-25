#[cfg(test)]
mod tests;

use crate::indexer_selection::{self, IndexerQuery, Indexers, SelectionError, UnresolvedBlock};
pub use crate::{
    indexer_selection::{Indexing, UtilityConfig},
    prelude::*,
};
use async_trait::async_trait;
pub use graphql_client::Response;
use im;
use lazy_static::lazy_static;
use prometheus;
use serde::{self, Deserialize, Serialize};
use serde_json::value::RawValue;
use std::{collections::HashMap, error::Error, sync::Arc};

#[derive(Clone, Debug)]
pub enum Subgraph {
    Name(String),
    Deployment(SubgraphDeploymentID),
}

#[derive(Clone, Debug)]
pub struct ClientQuery {
    pub id: u64,
    pub api_key: Arc<APIKey>,
    pub query: String,
    pub variables: Option<String>,
    pub network: String,
    pub subgraph: Subgraph,
}

#[derive(Clone, Debug, Default)]
pub struct APIKey {
    pub id: i64,
    pub key: String,
    pub user_id: i64,
    pub user_address: Address,
    pub queries_activated: bool,
    pub deployments: Vec<SubgraphDeploymentID>,
    pub subgraphs: Vec<(String, i32)>,
    pub domains: Vec<(String, i32)>,
}

#[derive(Debug)]
pub struct QueryResponse {
    pub query: IndexerQuery,
    pub response: IndexerResponse,
}

#[derive(Debug, Deserialize)]
pub struct IndexerResponse {
    #[serde(rename(deserialize = "graphQLResponse"))]
    pub graphql_response: String,
    pub attestation: Attestation,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Attestation {
    #[serde(rename(deserialize = "requestCID"))]
    request_cid: Bytes32,
    #[serde(rename(deserialize = "responseCID"))]
    response_cid: Bytes32,
    #[serde(rename(deserialize = "subgraphDeploymentID"))]
    deployment: Bytes32,
    v: u8,
    r: Bytes32,
    s: Bytes32,
}

#[derive(Debug)]
pub enum QueryEngineError {
    SubgraphNotFound,
    NoIndexerSelected,
    APIKeySubgraphNotAuthorized,
    MalformedQuery,
    MissingBlocks(Vec<UnresolvedBlock>),
}

struct Metrics {
    block_resolution_duration: prometheus::HistogramVec,
    block_resolution_requests_failed: prometheus::IntCounterVec,
    block_resolution_requests_ok: prometheus::IntCounterVec,
    indexer_requests_duration: prometheus::HistogramVec,
    indexer_requests_failed: prometheus::IntCounterVec,
    indexer_requests_ok: prometheus::IntCounterVec,
    indexer_selection: IndexerSelectionMetrics,
    indexer_selection_duration: prometheus::HistogramVec,
    queries_failed: prometheus::IntCounterVec,
    queries_ok: prometheus::IntCounterVec,
    queries_unauthorized_deployment: prometheus::IntCounterVec,
    query_duration: prometheus::HistogramVec,
    query_execution_duration: prometheus::HistogramVec,
    subgraph_name_duration: prometheus::HistogramVec,
}

struct IndexerSelectionMetrics {
    blocks_behind: prometheus::HistogramVec,
    fee: prometheus::HistogramVec,
    indexer_selected: prometheus::IntCounterVec,
    slashable_dollars: prometheus::HistogramVec,
    utility: prometheus::HistogramVec,
}

lazy_static! {
    static ref METRICS: Metrics = Metrics::new();
}

#[async_trait]
pub trait Resolver {
    async fn resolve_blocks(&self, network: &str, unresolved: &[UnresolvedBlock])
        -> Vec<BlockHead>;

    async fn query_indexer(&self, query: &IndexerQuery) -> Result<IndexerResponse, Box<dyn Error>>;

    async fn create_transfer(
        &self,
        indexers: &Indexers,
        indexing: Indexing,
        fee: GRT,
    ) -> Result<(), Box<dyn Error>>;
}

#[derive(Clone)]
pub struct Config {
    pub indexer_selection_retry_limit: usize,
    pub utility: UtilityConfig,
    pub query_budget: GRT,
}

#[derive(Clone)]
pub struct Inputs {
    pub indexers: Arc<Indexers>,
    pub current_deployments: Eventual<Ptr<HashMap<String, SubgraphDeploymentID>>>,
    pub deployment_indexers: Eventual<Ptr<HashMap<SubgraphDeploymentID, im::Vector<Address>>>>,
}

pub struct InputWriters {
    pub indexer_inputs: indexer_selection::InputWriters,
    pub indexers: Arc<Indexers>,
    pub current_deployments: EventualWriter<Ptr<HashMap<String, SubgraphDeploymentID>>>,
    pub deployment_indexers:
        EventualWriter<Ptr<HashMap<SubgraphDeploymentID, im::Vector<Address>>>>,
}

impl Inputs {
    pub fn new() -> (InputWriters, Self) {
        let (indexer_input_writers, indexer_inputs) = Indexers::inputs();
        let indexers = Arc::new(Indexers::new(indexer_inputs));
        let (current_deployments_writer, current_deployments) = Eventual::new();
        let (deployment_indexers_writer, deployment_indexers) = Eventual::new();
        (
            InputWriters {
                indexer_inputs: indexer_input_writers,
                indexers: indexers.clone(),
                current_deployments: current_deployments_writer,
                deployment_indexers: deployment_indexers_writer,
            },
            Inputs {
                indexers,
                current_deployments,
                deployment_indexers,
            },
        )
    }
}

pub struct QueryEngine<R: Clone + Resolver + Send> {
    indexers: Arc<Indexers>,
    current_deployments: Eventual<Ptr<HashMap<String, SubgraphDeploymentID>>>,
    deployment_indexers: Eventual<Ptr<HashMap<SubgraphDeploymentID, im::Vector<Address>>>>,
    resolver: R,
    config: Config,
}

impl<R: Clone + Resolver + Send + 'static> QueryEngine<R> {
    pub fn new(config: Config, resolver: R, inputs: Inputs) -> Self {
        Self {
            indexers: inputs.indexers,
            current_deployments: inputs.current_deployments,
            deployment_indexers: inputs.deployment_indexers,
            resolver,
            config,
        }
    }

    #[tracing::instrument(skip(self, query), fields(%query.id))]
    pub async fn execute_query(
        &self,
        query: ClientQuery,
    ) -> Result<QueryResponse, QueryEngineError> {
        tracing::debug!(
            query.network = %query.network,
            query.subgraph = ?query.subgraph,
            indexer_selection_retry_limit = ?self.config.indexer_selection_retry_limit
        );
        let api_key = query.api_key.key.clone();
        let query_start = Instant::now();
        let name_timer = if let Subgraph::Name(subgraph_name) = &query.subgraph {
            with_metric(&METRICS.subgraph_name_duration, &[subgraph_name], |hist| {
                hist.start_timer()
            })
        } else {
            None
        };
        let deployment = match &query.subgraph {
            Subgraph::Deployment(deployment) => deployment.clone(),
            Subgraph::Name(name) => self
                .current_deployments
                .value_immediate()
                .and_then(|map| map.get(name).cloned())
                .ok_or_else(|| QueryEngineError::SubgraphNotFound)?,
        };
        if !query.api_key.deployments.is_empty() && !query.api_key.deployments.contains(&deployment)
        {
            with_metric(
                &METRICS.queries_unauthorized_deployment,
                &[&api_key],
                |counter| counter.inc(),
            );
            return Err(QueryEngineError::APIKeySubgraphNotAuthorized);
        }
        name_timer.map(|t| t.observe_duration());
        let deployment_ipfs = deployment.ipfs_hash();
        let result = self
            .execute_deployment_query(query, deployment, &deployment_ipfs)
            .await;
        let result_counter = if let Ok(_) = result {
            &METRICS.queries_ok
        } else {
            &METRICS.queries_failed
        };
        with_metric(result_counter, &[&deployment_ipfs, &api_key], |counter| {
            counter.inc()
        });
        with_metric(
            &METRICS.query_duration,
            &[&deployment_ipfs, &api_key],
            |hist| {
                let t = Instant::now() - query_start;
                hist.observe(t.as_secs_f64());
            },
        );
        result
    }

    #[tracing::instrument(skip(self, query, deployment_ipfs), fields(%query.id))]
    async fn execute_deployment_query(
        &self,
        query: ClientQuery,
        deployment: SubgraphDeploymentID,
        deployment_ipfs: &str,
    ) -> Result<QueryResponse, QueryEngineError> {
        use QueryEngineError::*;
        let mut indexers = self
            .deployment_indexers
            .value_immediate()
            .and_then(|map| map.get(&deployment).cloned())
            .unwrap_or_default();
        tracing::debug!(?deployment, deployment_indexers = indexers.len());
        let _execution_timer = with_metric(
            &METRICS.query_execution_duration,
            &[&deployment_ipfs],
            |hist| hist.start_timer(),
        );
        for _ in 0..self.config.indexer_selection_retry_limit {
            let selection_timer = with_metric(
                &METRICS.indexer_selection_duration,
                &[&deployment_ipfs],
                |hist| hist.start_timer(),
            );
            let selection_result = self
                .indexers
                .select_indexer(
                    &self.config.utility,
                    &query.network,
                    &deployment,
                    &indexers,
                    query.query.clone(),
                    query.variables.clone(),
                    self.config.query_budget,
                )
                .await;

            selection_timer.map(|t| t.observe_duration());
            match &selection_result {
                Ok(None) => tracing::info!(err = ?NoIndexerSelected),
                Err(err) => tracing::info!(?err),
                _ => (),
            };
            let indexer_query = match selection_result {
                Ok(Some(indexer_query)) => indexer_query,
                Ok(None)
                | Err(SelectionError::MissingNetworkParams)
                | Err(SelectionError::BadIndexer(_))
                | Err(SelectionError::InsufficientCollateral(_, _)) => {
                    return Err(NoIndexerSelected)
                }
                Err(SelectionError::BadInput) => return Err(MalformedQuery),
                Err(SelectionError::MissingBlocks(unresolved)) => {
                    self.resolve_blocks(&query, unresolved).await?;
                    continue;
                }
            };
            if indexer_query.low_collateral_warning {
                let resolver = self.resolver.clone();
                let indexers = self.indexers.clone();
                let indexing = indexer_query.indexing.clone();
                let fee = indexer_query.fee.clone();
                tokio::spawn(
                    async move {
                        let res = resolver.create_transfer(&indexers, indexing, fee);
                        if let Err(transfer_err) = res.await {
                            tracing::error!(%transfer_err);
                        }
                    }
                    .in_current_span(),
                );
            }

            let indexer_id = indexer_query.indexing.indexer.to_string();
            tracing::info!(indexer = %indexer_id);
            self.observe_indexer_selection_metrics(&deployment, &indexer_query);
            let t0 = Instant::now();
            let result = self.resolver.query_indexer(&indexer_query).await;
            let query_duration = Instant::now() - t0;
            tracing::debug!(query_duration = ?query_duration);
            with_metric(
                &METRICS.indexer_requests_duration,
                &[&deployment_ipfs, &indexer_id],
                |hist| hist.observe(query_duration.as_secs_f64()),
            );

            let response = match result {
                Ok(response) => response,
                Err(indexer_response_err) => {
                    tracing::info!(%indexer_response_err);
                    self.indexers
                        .observe_failed_query(&indexer_query.indexing, &indexer_query.receipt, true)
                        .await;
                    indexers.remove(
                        indexers
                            .iter()
                            .position(|indexer| indexer == &indexer_query.indexing.indexer)
                            .unwrap(),
                    );
                    with_metric(
                        &METRICS.indexer_requests_failed,
                        &[&deployment_ipfs, &indexer_id],
                        |counter| counter.inc(),
                    );
                    continue;
                }
            };
            with_metric(
                &METRICS.indexer_requests_ok,
                &[&deployment_ipfs, &indexer_id],
                |counter| counter.inc(),
            );

            let indexer_behind_err =
                "Failed to decode `block.hash` value: `no block with that hash found`";
            if serde_json::from_str::<Response<Box<RawValue>>>(&response.graphql_response)
                .map_err(|_| QueryEngineError::NoIndexerSelected)?
                .errors
                .as_ref()
                .map(|errs| errs.iter().any(|err| err.message == indexer_behind_err))
                .unwrap_or(false)
            {
                tracing::info!("indexing behind");
                self.indexers.observe_indexing_behind(&indexer_query).await;
                continue;
            }

            // TODO: fisherman

            tracing::info!("query successful");
            self.indexers
                .observe_successful_query(
                    &indexer_query.indexing,
                    query_duration,
                    &indexer_query.receipt,
                )
                .await;
            return Ok(QueryResponse {
                query: indexer_query,
                response,
            });
        }
        Err(NoIndexerSelected)
    }

    async fn resolve_blocks(
        &self,
        query: &ClientQuery,
        mut unresolved: Vec<UnresolvedBlock>,
    ) -> Result<(), QueryEngineError> {
        let _block_resolution_timer = with_metric(
            &METRICS.block_resolution_duration,
            &[&query.network],
            |hist| hist.start_timer(),
        );
        tracing::debug!(unresolved_blocks = ?unresolved);
        let heads = self
            .resolver
            .resolve_blocks(&query.network, &unresolved)
            .await;
        for head in heads {
            unresolved.swap_remove(
                unresolved
                    .iter()
                    .position(|b| match b {
                        UnresolvedBlock::WithHash(h) => h == &head.block.hash,
                        UnresolvedBlock::WithNumber(n) => n == &head.block.number,
                    })
                    .unwrap(),
            );
        }
        if !unresolved.is_empty() {
            with_metric(
                &METRICS.block_resolution_requests_failed,
                &[&query.network],
                |counter| counter.inc(),
            );
            return Err(QueryEngineError::MissingBlocks(unresolved));
        }
        with_metric(
            &METRICS.block_resolution_requests_ok,
            &[&query.network],
            |counter| counter.inc(),
        );
        Ok(())
    }

    fn observe_indexer_selection_metrics(
        &self,
        deployment: &SubgraphDeploymentID,
        selection: &IndexerQuery,
    ) {
        let deployment = deployment.ipfs_hash();
        let metrics = &METRICS.indexer_selection;
        if let Ok(hist) = metrics
            .blocks_behind
            .get_metric_with_label_values(&[&deployment])
        {
            if let Some(blocks_behind) = selection.blocks_behind {
                hist.observe(blocks_behind as f64);
            }
        }
        if let Ok(hist) = metrics.fee.get_metric_with_label_values(&[&deployment]) {
            hist.observe(selection.fee.as_f64());
        }
        if let Ok(counter) = metrics
            .indexer_selected
            .get_metric_with_label_values(&[&deployment, &selection.indexing.indexer.to_string()])
        {
            counter.inc();
        }
        if let Ok(hist) = metrics
            .slashable_dollars
            .get_metric_with_label_values(&[&deployment])
        {
            hist.observe(selection.slashable_usd.as_f64());
        }
        if let Ok(hist) = metrics.utility.get_metric_with_label_values(&[&deployment]) {
            hist.observe(*selection.utility);
        }
    }
}

impl Metrics {
    fn new() -> Self {
        Self {
            block_resolution_duration: prometheus::register_histogram_vec!(
                "block_resolution_duration",
                "Duration of block requests",
                &["network"]
            )
            .unwrap(),
            block_resolution_requests_failed: prometheus::register_int_counter_vec!(
                "block_resolution_requests_failed",
                "Number of failed block requests",
                &["network"]
            )
            .unwrap(),
            block_resolution_requests_ok: prometheus::register_int_counter_vec!(
                "block_resolution_requests_ok",
                "Number of successful block requests",
                &["network"]
            )
            .unwrap(),
            indexer_requests_duration: prometheus::register_histogram_vec!(
                "query_engine_indexer_request_duration",
                "Duration of making a request to an indexer",
                &["deployment", "indexer"]
            )
            .unwrap(),
            indexer_requests_failed: prometheus::register_int_counter_vec!(
                "query_engine_indexer_requests_failed",
                "Number of failed queries made to an indexer",
                &["deployment", "indexer"]
            )
            .unwrap(),
            indexer_requests_ok: prometheus::register_int_counter_vec!(
                "query_engine_indexer_requests_ok",
                "Number of successful requests made to an indexer",
                &["deployment", "indexer"]
            )
            .unwrap(),
            indexer_selection: IndexerSelectionMetrics {
                blocks_behind: prometheus::register_histogram_vec!(
                    "indexer_selection_blocks_behind",
                    "Number of blocks that indexers are behind",
                    &["deployment"]
                )
                .unwrap(),
                fee: prometheus::register_histogram_vec!(
                    "indexer_selection_fee",
                    "Query fee amount based on cost models",
                    &["deployment"]
                )
                .unwrap(),
                indexer_selected: prometheus::register_int_counter_vec!(
                    "indexer_selection_indexer_selected",
                    "Number of times an indexer was selected",
                    &["deployment", "indexer"]
                )
                .unwrap(),
                slashable_dollars: prometheus::register_histogram_vec!(
                    "indexer_selection_slashable_dollars",
                    "Slashable dollars of selected indexers",
                    &["deployment"]
                )
                .unwrap(),
                utility: prometheus::register_histogram_vec!(
                    "indexer_selection_utility",
                    "Combined overall utility",
                    &["deployment"]
                )
                .unwrap(),
            },
            indexer_selection_duration: prometheus::register_histogram_vec!(
                "query_engine_indexer_selection_duration",
                "Duration of selecting an indexer for a query",
                &["deployment"]
            )
            .unwrap(),
            queries_failed: prometheus::register_int_counter_vec!(
                "gateway_queries_failed",
                "Queries that failed executing",
                &["deployment", "apiKey"]
            )
            .unwrap(),
            queries_ok: prometheus::register_int_counter_vec!(
                "gateway_queries_ok",
                "Successfully executed queries",
                &["deployment", "apiKey"]
            )
            .unwrap(),
            queries_unauthorized_deployment: prometheus::register_int_counter_vec!(
                "gateway_queries_for_excluded_deployment",
                "Queries for a subgraph deployment not included in an API key",
                &["apiKey"]
            )
            .unwrap(),
            query_duration: prometheus::register_histogram_vec!(
                "gateway_query_execution_duration",
                "Duration of processing a query",
                &["deployment", "apiKey"]
            )
            .unwrap(),
            query_execution_duration: prometheus::register_histogram_vec!(
                "query_engine_query_execution_duration",
                "Duration of executing the query for a deployment",
                &["deployment"]
            )
            .unwrap(),
            subgraph_name_duration: prometheus::register_histogram_vec!(
                "query_engine_subgraph_name_duration",
                "Duration of resolving a subgraph name to a deployment",
                &["name"]
            )
            .unwrap(),
        }
    }
}

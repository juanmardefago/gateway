use crate::{
    actor::{process_updates, IndexerUpdate, Update},
    test_utils::default_cost_model,
    BlockRequirements, BlockStatus, Candidate, Context, IndexerError, IndexerErrors, IndexerInfo,
    Indexing, IndexingStatus, InputError, Selection, UtilityParameters, SELECTION_LIMIT,
};
use num_traits::ToPrimitive as _;
use prelude::{
    anyhow::{bail, ensure},
    buffer_queue::QueueWriter,
    rand::{
        rngs::SmallRng,
        seq::{IteratorRandom, SliceRandom},
        thread_rng, Rng, RngCore as _, SeedableRng as _,
    },
    test_utils::*,
    tokio::spawn,
    *,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env,
    ops::RangeInclusive,
    sync::Arc,
};

#[derive(Clone)]
struct Config {
    blocks: RangeInclusive<usize>,
    indexers: RangeInclusive<usize>,
    deployments: RangeInclusive<usize>,
    indexings: RangeInclusive<usize>,
}

#[derive(Debug)]
struct Topology {
    usd_to_grt_conversion: GRT,
    slashing_percentage: PPM,
    blocks: Vec<BlockPointer>,
    indexers: HashMap<Address, Arc<IndexerInfo>>,
    deployments: HashSet<DeploymentId>,
    indexings: HashMap<Indexing, IndexingStatus>,
}

#[derive(Debug)]
struct Request {
    deployment: DeploymentId,
    indexers: Vec<Address>,
    params: UtilityParameters,
    query: String,
    selection_limit: u8,
}

impl Topology {
    async fn gen(
        rng: &mut SmallRng,
        config: &Config,
        update_writer: &mut QueueWriter<Update>,
    ) -> Self {
        let indexers = (0..rng.gen_range(config.indexers.clone()))
            .map(|id| (Address(bytes_from_id(id)), Self::gen_indexer_info(rng)))
            .collect();
        let deployments = (0..rng.gen_range(config.deployments.clone()))
            .map(|id| DeploymentId(bytes_from_id(id)))
            .collect();
        let blocks = (0..rng.gen_range(config.blocks.clone()))
            .map(|id| BlockPointer {
                number: id as u64,
                hash: Bytes32(bytes_from_id(id)),
            })
            .collect::<Vec<BlockPointer>>();
        let indexings = (0..rng.gen_range(config.indexings.clone()))
            .filter_map(|_| Self::gen_indexing(rng, &blocks, &indexers, &deployments))
            .collect();
        let state = Self {
            usd_to_grt_conversion: "1.0".parse().unwrap(),
            slashing_percentage: "0.1".parse().unwrap(),
            blocks,
            indexers,
            deployments,
            indexings,
        };

        update_writer
            .write(Update::USDToGRTConversion(state.usd_to_grt_conversion))
            .unwrap();
        update_writer
            .write(Update::SlashingPercentage(state.slashing_percentage))
            .unwrap();
        update_writer
            .write(Update::Indexers(
                state
                    .indexers
                    .iter()
                    .map(|(indexer, info)| {
                        (
                            *indexer,
                            IndexerUpdate {
                                info: info.clone(),
                                indexings: state
                                    .indexings
                                    .iter()
                                    .filter(|(indexing, _)| &indexing.indexer == indexer)
                                    .map(|(indexing, status)| (indexing.deployment, status.clone()))
                                    .collect(),
                            },
                        )
                    })
                    .collect(),
            ))
            .unwrap();

        update_writer.flush().await.unwrap();
        state
    }

    fn gen_indexer_info(rng: &mut SmallRng) -> Arc<IndexerInfo> {
        Arc::new(IndexerInfo {
            url: "http://localhost:8000".parse().unwrap(),
            stake: Self::gen_grt(rng, &[0.0, 50e3, 100e3, 150e3]),
        })
    }

    fn gen_indexing(
        rng: &mut SmallRng,
        blocks: &[BlockPointer],
        indexers: &HashMap<Address, Arc<IndexerInfo>>,
        deployments: &HashSet<DeploymentId>,
    ) -> Option<(Indexing, IndexingStatus)> {
        let indexing = Indexing {
            indexer: *indexers.iter().choose(rng)?.0,
            deployment: *deployments.iter().choose(rng)?,
        };
        let status = IndexingStatus {
            allocations: Arc::new(HashMap::from_iter([(
                Address(bytes_from_id(0)),
                "1".parse().unwrap(),
            )])),
            cost_model: Some(Ptr::new(default_cost_model(Self::gen_grt(
                rng,
                &[0.0, 0.1, 1.0, 2.0],
            )))),
            block: blocks.choose(rng).map(|b| BlockStatus {
                reported_number: b.number,
                blocks_behind: blocks.len() as u64 - 1 - b.number,
                behind_reported_block: false,
                min_block: None,
            }),
        };
        Some((indexing, status))
    }

    fn gen_grt(rng: &mut SmallRng, table: &[f64; 4]) -> GRT {
        GRT::try_from(*table.choose(rng).unwrap()).unwrap()
    }

    fn gen_request(&self, rng: &mut SmallRng) -> Option<Request> {
        let deployment = *self.deployments.iter().choose(rng)?;
        let required_block = match (rng.gen_bool(0.1), self.blocks.choose(rng)) {
            (true, Some(block)) => Some(block.number),
            _ => None,
        };
        Some(Request {
            deployment,
            indexers: self
                .indexings
                .keys()
                .filter(|indexing| indexing.deployment == deployment)
                .map(|indexing| indexing.indexer)
                .collect(),
            params: UtilityParameters::new(
                "1.0".parse().unwrap(),
                BlockRequirements {
                    range: required_block.map(|b| (0, b)),
                    has_latest: required_block.is_some() && rng.gen_bool(0.5),
                },
                self.blocks.last()?.number,
                0.1,
                0.0,
                0.0,
                0.0,
                0.0,
            ),
            query: "{ entities { id } }".to_string(),
            selection_limit: rng.gen_range(1..=SELECTION_LIMIT) as u8,
        })
    }

    fn check(
        &self,
        request: &Request,
        result: &Result<(Vec<Selection>, IndexerErrors<'_>), InputError>,
    ) -> anyhow::Result<()> {
        let (selections, errors) = match result {
            Ok((selections, errors)) => (selections, errors),
            Err(_) => bail!("unexpected InputError"),
        };

        let mut context = Context::new(&request.query, "").unwrap();

        let fees = selections
            .iter()
            .map(|s| s.fee)
            .fold(GRT::zero(), |sum, fee| sum + fee);
        ensure!(fees <= request.params.budget);

        let mut expected_errors = IndexerErrors(BTreeMap::new());
        for indexer in &request.indexers {
            let info = self.indexers.get(indexer).unwrap();
            let status = self
                .indexings
                .get(&Indexing {
                    indexer: *indexer,
                    deployment: request.deployment,
                })
                .unwrap();
            let total_allocation = status
                .allocations
                .iter()
                .map(|(_, size)| size)
                .fold(GRT::zero(), |sum, size| sum + *size);
            let required_block = request.params.requirements.range.map(|(_, n)| n);
            let fee = status
                .cost_model
                .as_ref()
                .map(|c| c.cost_with_context(&mut context).unwrap().to_f64().unwrap())
                .unwrap_or(0.0)
                / 1e18;
            println!("indexer={}, fee={}", indexer, fee);
            let mut set_err = |err: IndexerError| {
                expected_errors.0.entry(err).or_default().insert(indexer);
            };

            if matches!(required_block, Some(n) if n > status.block.as_ref().unwrap().reported_number)
            {
                set_err(IndexerError::MissingRequiredBlock);
            } else if status.block.is_none() {
                set_err(IndexerError::NoStatus);
            } else if info.stake == GRT::zero() {
                set_err(IndexerError::NoStake);
            } else if total_allocation == GRT::zero() {
                set_err(IndexerError::NoAllocation);
            } else if fee > request.params.budget.as_f64() {
                set_err(IndexerError::FeeTooHigh);
            }
        }
        println!("{:#?}", expected_errors);

        ensure!(errors.0 == expected_errors.0);
        // An indexer must be selected if one exists without an associated error.
        if selections.is_empty() {
            ensure!(errors.0.iter().map(|(_, s)| s.len()).sum::<usize>() == request.indexers.len());
        }

        Ok(())
    }
}

#[tokio::test]
async fn fuzz() {
    let seed = env::vars()
        .find(|(k, _)| k == "TEST_SEED")
        .and_then(|(_, v)| v.parse::<u64>().ok())
        .unwrap_or(thread_rng().next_u64());
    println!("TEST_SEED={}", seed);
    let mut rng = SmallRng::seed_from_u64(seed);

    let (isa_state, isa_writer) = double_buffer!(crate::State::default());
    let (mut update_writer, update_reader) = buffer_queue::pair();
    spawn(async move {
        process_updates(isa_writer, update_reader).await;
        panic!("ISA actor stopped");
    });

    let config = Config {
        blocks: 0..=5,
        indexers: 0..=3,
        deployments: 0..=3,
        indexings: 0..=5,
    };

    for _ in 0..100 {
        let topology = Topology::gen(&mut rng, &config, &mut update_writer).await;
        println!("{:#?}", topology);
        let request = match topology.gen_request(&mut rng) {
            Some(request) => request,
            None => continue,
        };
        println!("{:#?}", request);
        let mut context = Context::new(&request.query, "").unwrap();
        let candidates: Vec<Candidate> = request
            .indexers
            .iter()
            .map(|indexer| Candidate {
                indexing: Indexing {
                    indexer: *indexer,
                    deployment: request.deployment,
                },
                versions_behind: 0,
            })
            .collect();
        let result = isa_state.latest().select_indexers(
            &candidates,
            &request.params,
            &mut context,
            request.selection_limit,
        );
        println!("{:#?}", result);
        if let Err(err) = topology.check(&request, &result) {
            println!("{}", err);
            break;
        }
    }
}

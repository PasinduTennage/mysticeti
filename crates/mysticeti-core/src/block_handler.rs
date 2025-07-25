// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{HashMap, HashSet},
    env,
    path::Path,
    sync::Arc,
    time::Duration,
};

use minibytes::Bytes;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{Receiver, Sender};

use crate::{
    block_store::BlockStore,
    committee::{Committee, ProcessedTransactionHandler, QuorumThreshold, TransactionAggregator},
    consensus::linearizer::{CommittedSubDag, Linearizer},
    data::Data,
    log::TransactionLog,
    metrics::{Metrics, UtilizationTimerExt, UtilizationTimerVecExt},
    runtime::{self, TimeInstant},
    syncer::CommitObserver,
    transactions_generator::TransactionGenerator,
    types::{
        AuthorityIndex,
        BaseStatement,
        BlockReference,
        StatementBlock,
        Transaction,
        TransactionLocator,
    },
};

use crate::runtime::timestamp_utc;

pub trait BlockHandler: Send + Sync {
    fn handle_blocks(
        &mut self,
        blocks: &[Data<StatementBlock>],
        require_response: bool,
    ) -> Vec<BaseStatement>;

    fn handle_proposal(&mut self, block: &Data<StatementBlock>);

    fn state(&self) -> Bytes;

    fn recover_state(&mut self, _state: &Bytes);

    fn cleanup(&self) {}
}

const REAL_BLOCK_HANDLER_TXN_SIZE: usize = 512;
const REAL_BLOCK_HANDLER_TXN_GEN_STEP: usize = 32;
const _: () = assert_constants();

#[allow(dead_code)]
const fn assert_constants() {
    if REAL_BLOCK_HANDLER_TXN_SIZE % REAL_BLOCK_HANDLER_TXN_GEN_STEP != 0 {
        panic!("REAL_BLOCK_HANDLER_TXN_SIZE % REAL_BLOCK_HANDLER_TXN_GEN_STEP != 0")
    }
}

pub struct RealBlockHandler {
    transaction_votes: TransactionAggregator<QuorumThreshold, TransactionLog>,
    pub transaction_time: Arc<Mutex<HashMap<TransactionLocator, TimeInstant>>>,
    committee: Arc<Committee>,
    authority: AuthorityIndex,
    block_store: BlockStore,
    metrics: Arc<Metrics>,
    receiver: mpsc::Receiver<Vec<Transaction>>,
    pending_transactions: usize,
    consensus_only: bool,
}

/// The max number of transactions per block.
// todo - This value should be in bytes because it is capped by the wal entry size.
pub const SOFT_MAX_PROPOSED_PER_BLOCK: usize = 20 * 1000;

impl RealBlockHandler {
    pub fn new(
        committee: Arc<Committee>,
        authority: AuthorityIndex,
        certified_transactions_log_path: &Path,
        block_store: BlockStore,
        metrics: Arc<Metrics>,
        consensus_only: bool,
    ) -> (Self, mpsc::Sender<Vec<Transaction>>) {
        let (sender, receiver) = mpsc::channel(1024);
        let transaction_log = TransactionLog::start(certified_transactions_log_path)
            .expect("Failed to open certified transaction log for write");

        let this = Self {
            transaction_votes: TransactionAggregator::with_handler(transaction_log),
            transaction_time: Default::default(),
            committee,
            authority,
            block_store,
            metrics,
            receiver,
            pending_transactions: 0, // todo - need to initialize correctly when loaded from disk
            consensus_only,
        };
        (this, sender)
    }
}

impl RealBlockHandler {
    fn receive_with_limit(&mut self) -> Option<Vec<Transaction>> {
        if self.pending_transactions >= SOFT_MAX_PROPOSED_PER_BLOCK {
            return None;
        }
        let received = self.receiver.try_recv().ok()?;
        self.pending_transactions += received.len();
        Some(received)
    }

    /// Expose a metric for certified transactions.
    fn update_metrics(
        &self,
        block_creation: Option<&TimeInstant>,
        transaction: &Transaction,
        current_timestamp: &Duration,
    ) {
        // Record inter-block latency.
        if let Some(instant) = block_creation {
            let latency = instant.elapsed();
            self.metrics.transaction_certified_latency.observe(latency);
            self.metrics
                .inter_block_latency_s
                .with_label_values(&["owned"])
                .observe(latency.as_secs_f64());
        }

        // Record end-to-end latency.
        let tx_submission_timestamp = TransactionGenerator::extract_timestamp(transaction);
        let latency = current_timestamp.saturating_sub(tx_submission_timestamp);
        let square_latency = latency.as_secs_f64().powf(2.0);
        self.metrics
            .latency_s
            .with_label_values(&["owned"])
            .observe(latency.as_secs_f64());
        self.metrics
            .latency_squared_s
            .with_label_values(&["owned"])
            .inc_by(square_latency);
    }
}

impl BlockHandler for RealBlockHandler {
    fn handle_blocks(
        &mut self,
        blocks: &[Data<StatementBlock>],
        require_response: bool,
    ) -> Vec<BaseStatement> {
        let current_timestamp = runtime::timestamp_utc();
        let _timer = self
            .metrics
            .utilization_timer
            .utilization_timer("BlockHandler::handle_blocks");
        let mut response = vec![];
        if require_response {
            while let Some(data) = self.receive_with_limit() {
                for tx in data {
                    response.push(BaseStatement::Share(tx));
                }
            }
        }
        let transaction_time = self.transaction_time.lock();
        for block in blocks {
            let response_option: Option<&mut Vec<BaseStatement>> = if require_response {
                Some(&mut response)
            } else {
                None
            };
            if !self.consensus_only {
                let processed =
                    self.transaction_votes
                        .process_block(block, response_option, &self.committee);
                for processed_locator in processed {
                    let block_creation = transaction_time.get(&processed_locator);
                    let transaction = self
                        .block_store
                        .get_transaction(&processed_locator)
                        .expect("Failed to get certified transaction");
                    self.update_metrics(block_creation, &transaction, &current_timestamp);
                }
            }
        }
        self.metrics
            .block_handler_pending_certificates
            .set(self.transaction_votes.len() as i64);
        response
    }

    fn handle_proposal(&mut self, block: &Data<StatementBlock>) {
        // todo - this is not super efficient
        self.pending_transactions -= block.shared_transactions().count();
        let mut transaction_time = self.transaction_time.lock();
        for (locator, _) in block.shared_transactions() {
            transaction_time.insert(locator, TimeInstant::now());
        }
        if !self.consensus_only {
            for range in block.shared_ranges() {
                self.transaction_votes
                    .register(range, self.authority, &self.committee);
            }
        }
    }

    fn state(&self) -> Bytes {
        self.transaction_votes.state()
    }

    fn recover_state(&mut self, state: &Bytes) {
        self.transaction_votes.with_state(state);
    }

    fn cleanup(&self) {
        let _timer = self.metrics.block_handler_cleanup_util.utilization_timer();
        // todo - all of this should go away and we should measure tx latency differently
        let mut l = self.transaction_time.lock();
        l.retain(|_k, v| v.elapsed() < Duration::from_secs(10));
    }
}

// Immediately votes and generates new transactions
pub struct TestBlockHandler {
    last_transaction: u64,
    transaction_votes: TransactionAggregator<QuorumThreshold>,
    pub transaction_time: Arc<Mutex<HashMap<TransactionLocator, TimeInstant>>>,
    committee: Arc<Committee>,
    authority: AuthorityIndex,
    pub proposed: Vec<TransactionLocator>,

    metrics: Arc<Metrics>,
}

impl TestBlockHandler {
    pub fn new(
        last_transaction: u64,
        committee: Arc<Committee>,
        authority: AuthorityIndex,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            last_transaction,
            transaction_votes: Default::default(),
            transaction_time: Default::default(),
            committee,
            authority,
            proposed: Default::default(),
            metrics,
        }
    }

    pub fn is_certified(&self, locator: &TransactionLocator) -> bool {
        self.transaction_votes.is_processed(locator)
    }

    pub fn make_transaction(i: u64) -> Transaction {
        Transaction::new(i.to_le_bytes().to_vec())
    }
}

impl BlockHandler for TestBlockHandler {
    fn handle_blocks(
        &mut self,
        blocks: &[Data<StatementBlock>],
        require_response: bool,
    ) -> Vec<BaseStatement> {
        // todo - this is ugly, but right now we need a way to recover self.last_transaction
        let mut response = vec![];
        if require_response {
            for block in blocks {
                if block.author() == self.authority {
                    // We can see our own block in handle_blocks - this can happen during core recovery
                    // Todo - we might also need to process pending Payload statements as well
                    for statement in block.statements() {
                        if let BaseStatement::Share(_) = statement {
                            self.last_transaction += 1;
                        }
                    }
                }
            }
            self.last_transaction += 1;
            let next_transaction = Self::make_transaction(self.last_transaction);
            response.push(BaseStatement::Share(next_transaction));
        }
        let transaction_time = self.transaction_time.lock();
        for block in blocks {
            tracing::debug!("Processing {block:?}");
            let response_option: Option<&mut Vec<BaseStatement>> = if require_response {
                Some(&mut response)
            } else {
                None
            };
            let processed =
                self.transaction_votes
                    .process_block(block, response_option, &self.committee);
            for processed_locator in processed {
                if let Some(instant) = transaction_time.get(&processed_locator) {
                    self.metrics
                        .transaction_certified_latency
                        .observe(instant.elapsed());
                }
            }
        }
        response
    }

    fn handle_proposal(&mut self, block: &Data<StatementBlock>) {
        let mut transaction_time = self.transaction_time.lock();
        for (locator, _) in block.shared_transactions() {
            transaction_time.insert(locator, TimeInstant::now());
            self.proposed.push(locator);
        }
        for range in block.shared_ranges() {
            self.transaction_votes
                .register(range, self.authority, &self.committee);
        }
    }

    fn state(&self) -> Bytes {
        let state = (&self.transaction_votes.state(), &self.last_transaction);
        let bytes =
            bincode::serialize(&state).expect("Failed to serialize transaction aggregator state");
        bytes.into()
    }

    fn recover_state(&mut self, state: &Bytes) {
        let (transaction_votes, last_transaction) = bincode::deserialize(state)
            .expect("Failed to deserialize transaction aggregator state");
        self.transaction_votes.with_state(&transaction_votes);
        self.last_transaction = last_transaction;
    }
}

pub struct TestCommitHandler<H = HashSet<TransactionLocator>> {
    commit_interpreter: Linearizer,
    transaction_votes: TransactionAggregator<QuorumThreshold, H>,
    committee: Arc<Committee>,
    committed_leaders: Vec<BlockReference>,
    // committed_dags: Vec<CommittedSubDag>,
    start_time: TimeInstant,
    transaction_time: Arc<Mutex<HashMap<TransactionLocator, TimeInstant>>>,

    metrics: Arc<Metrics>,
    consensus_only: bool,
    tx: Option<Sender<(u128, u128)>>,
    start_time_duration: Duration,
}

impl<H: ProcessedTransactionHandler<TransactionLocator> + Default> TestCommitHandler<H> {
    pub fn new(
        committee: Arc<Committee>,
        transaction_time: Arc<Mutex<HashMap<TransactionLocator, TimeInstant>>>,
        metrics: Arc<Metrics>,
        authority: AuthorityIndex,
    ) -> Self {
        Self::new_with_handler(committee, transaction_time, metrics, Default::default(), authority)
    }
}

impl<H: ProcessedTransactionHandler<TransactionLocator>> TestCommitHandler<H> {
    pub fn new_with_handler(
        committee: Arc<Committee>,
        transaction_time: Arc<Mutex<HashMap<TransactionLocator, TimeInstant>>>,
        metrics: Arc<Metrics>,
        handler: H,
        authority: AuthorityIndex,
    ) -> Self {

        let (tx, mut rx): (Sender<(u128, u128)>, Receiver<(u128, u128)>) = mpsc::channel(10000000);
        let file_name = format!("client-times-{}.txt", authority);
        // start a new asynchronous task using the receiver (rx)
        tokio::spawn(async move {
            let mut file = match tokio::fs::File::create(&file_name).await {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("Failed to create file {}: {}", file_name, e);
                    return;
                }
            };
            let mut counter = 0;
            let mut pending = String::from("");
            // Continuously receive messages from the channel and write them to the file
            while let Some((start, end)) = rx.recv().await {
                let output = format!("{:?}, {:?}\n", start, end);
                pending.push_str(&output);
                counter = counter + 1;
                if counter == 1000000 {
                    tokio::io::AsyncWriteExt::write_all(&mut file, pending.as_bytes()).await;
                    counter = 0;
                    pending.clear();
                }
            }
        });

        let consensus_only = env::var("CONSENSUS_ONLY").is_ok();
        Self {
            commit_interpreter: Linearizer::new(),
            transaction_votes: TransactionAggregator::with_handler(handler),
            committee,
            committed_leaders: vec![],
            // committed_dags: vec![],
            start_time: TimeInstant::now(),
            transaction_time,

            metrics,
            consensus_only,
            tx: Some(tx.clone()),
            start_time_duration: timestamp_utc(),
        }
    }

    pub fn committed_leaders(&self) -> &Vec<BlockReference> {
        &self.committed_leaders
    }

    /// Note: these metrics are used to compute performance during benchmarks.
    fn update_metrics(
        &self,
        block_creation: Option<&TimeInstant>,
        current_timestamp: Duration,
        transaction: &Transaction,
    ) {
        // Record inter-block latency.
        if let Some(instant) = block_creation {
            let latency = instant.elapsed();
            self.metrics.transaction_committed_latency.observe(latency);
            self.metrics
                .inter_block_latency_s
                .with_label_values(&["shared"])
                .observe(latency.as_secs_f64());
        }

        // Record benchmark start time.
        let time_from_start = self.start_time.elapsed();
        let benchmark_duration = self.metrics.benchmark_duration.get();
        if let Some(delta) = time_from_start.as_secs().checked_sub(benchmark_duration) {
            self.metrics.benchmark_duration.inc_by(delta);
        }

        // Record end-to-end latency. The first 8 bytes of the transaction are the timestamp of the
        // transaction submission.
        let tx_submission_timestamp = TransactionGenerator::extract_timestamp(transaction);
        let latency = current_timestamp.saturating_sub(tx_submission_timestamp);
        let square_latency = latency.as_secs_f64().powf(2.0);
        self.metrics
            .latency_s
            .with_label_values(&["shared"])
            .observe(latency.as_secs_f64());
        self.metrics
            .latency_squared_s
            .with_label_values(&["shared"])
            .inc_by(square_latency);
        if let Some(tx) = &self.tx {
            let result = tx.try_send((
                tx_submission_timestamp
                    .checked_sub(self.start_time_duration)
                    .unwrap_or_default()
                    .as_micros(),
                current_timestamp
                    .checked_sub(self.start_time_duration)
                    .unwrap_or_default()
                    .as_micros(),
            ));
            if let Err(e) = result {
                eprintln!("Failed to send timestamp: {}", e);
            }
        }
    }
}

impl<H: ProcessedTransactionHandler<TransactionLocator> + Send + Sync> CommitObserver
    for TestCommitHandler<H>
{
    fn handle_commit(
        &mut self,
        block_store: &BlockStore,
        committed_leaders: Vec<Data<StatementBlock>>,
    ) -> Vec<CommittedSubDag> {
        let current_timestamp = runtime::timestamp_utc();

        let committed = self
            .commit_interpreter
            .handle_commit(block_store, committed_leaders);
        let transaction_time = self.transaction_time.lock();
        for commit in &committed {
            self.committed_leaders.push(commit.anchor);
            for block in &commit.blocks {
                if !self.consensus_only {
                    let processed =
                        self.transaction_votes
                            .process_block(block, None, &self.committee);
                    for processed_locator in processed {
                        if let Some(instant) = transaction_time.get(&processed_locator) {
                            // todo - batch send data points
                            self.metrics
                                .certificate_committed_latency
                                .observe(instant.elapsed());
                        }
                    }
                }
                for (locator, transaction) in block.shared_transactions() {
                    self.update_metrics(
                        transaction_time.get(&locator),
                        current_timestamp,
                        transaction,
                    );
                }
            }
            // self.committed_dags.push(commit);
        }
        self.metrics
            .commit_handler_pending_certificates
            .set(self.transaction_votes.len() as i64);
        committed
    }

    fn aggregator_state(&self) -> Bytes {
        self.transaction_votes.state()
    }

    fn recover_committed(&mut self, committed: HashSet<BlockReference>, state: Option<Bytes>) {
        assert!(self.commit_interpreter.committed.is_empty());
        if let Some(state) = state {
            self.transaction_votes.with_state(&state);
        } else {
            assert!(committed.is_empty());
        }
        self.commit_interpreter.committed = committed;
    }
}

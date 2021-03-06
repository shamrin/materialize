// Copyright Materialize, Inc. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Coordination of installed views, available timestamps, and compacted timestamps.
//!
//! The command coordinator maintains a view of the installed views, and for each tracks
//! the frontier of available times (`upper`) and the frontier of compacted times (`since`).
//! The upper frontier describes times that may not return immediately, as any timestamps in
//! advance of the frontier are still open. The since frontier constrains those times for
//! which the maintained view will be correct, as any timestamps in advance of the frontier
//! must accumulate to the same value as would an un-compacted trace.

use std::cmp;
use std::collections::{BTreeMap, HashMap};
use std::convert::{TryFrom, TryInto};
use std::iter;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, bail, Context};
use differential_dataflow::lattice::Lattice;
use futures::future::{self, TryFutureExt};
use futures::sink::SinkExt;
use futures::stream::{self, StreamExt, TryStreamExt};
use timely::progress::{Antichain, ChangeBatch, Timestamp as _};
use tokio::runtime::{Handle, Runtime};
use tokio_postgres::error::SqlState;
use uuid::Uuid;

use build_info::BuildInfo;
use dataflow::source::cache::CacheSender;
use dataflow::{CacheMessage, SequencedCommand, WorkerFeedback, WorkerFeedbackWithMeta};
use dataflow_types::logging::LoggingConfig as DataflowLoggingConfig;
use dataflow_types::{
    AvroOcfSinkConnector, DataflowDesc, IndexDesc, KafkaSinkConnector, PeekResponse, SinkConnector,
    SourceConnector, TailSinkConnector, TimestampSourceUpdate, Update,
};
use expr::{
    ExprHumanizer, GlobalId, Id, NullaryFunc, OptimizedRelationExpr, RelationExpr, RowSetFinishing,
    ScalarExpr, SourceInstanceId,
};
use ore::collections::CollectionExt;
use ore::thread::JoinHandleExt;
use repr::{ColumnName, Datum, RelationDesc, RelationType, Row, RowPacker, Timestamp};
use sql::ast::display::AstDisplay;
use sql::ast::{
    CreateIndexStatement, CreateTableStatement, DropObjectsStatement, ExplainOptions, ExplainStage,
    FetchStatement, ObjectType, Statement,
};
use sql::catalog::Catalog as _;
use sql::names::{DatabaseSpecifier, FullName, SchemaName};
use sql::plan::StatementDesc;
use sql::plan::{
    AlterIndexLogicalCompactionWindow, CopyFormat, LogicalCompactionWindow, MutationKind, Params,
    PeekWhen, Plan, PlanContext,
};
use transform::Optimizer;

use self::arrangement_state::{ArrangementFrontiers, Frontiers};
use crate::cache::{CacheConfig, Cacher};
use crate::catalog::builtin::{
    BUILTINS, MZ_ARRAY_TYPES, MZ_AVRO_OCF_SINKS, MZ_BASE_TYPES, MZ_COLUMNS, MZ_DATABASES,
    MZ_INDEXES, MZ_INDEX_COLUMNS, MZ_KAFKA_SINKS, MZ_LIST_TYPES, MZ_MAP_TYPES, MZ_SCHEMAS,
    MZ_SINKS, MZ_SOURCES, MZ_TABLES, MZ_TYPES, MZ_VIEWS, MZ_VIEW_FOREIGN_KEYS, MZ_VIEW_KEYS,
};
use crate::catalog::{self, Catalog, CatalogItem, Index, SinkConnectorState, Type, TypeInner};
use crate::command::{
    Command, ExecuteResponse, NoSessionExecuteResponse, Response, StartupMessage,
};
use crate::session::{PreparedStatement, Session, TransactionStatus};
use crate::sink_connector;
use crate::timestamp::{TimestampConfig, TimestampMessage, Timestamper};
use crate::util::ClientTransmitter;

mod arrangement_state;
mod dataflow_builder;

pub enum Message {
    Command(Command),
    Worker(WorkerFeedbackWithMeta),
    AdvanceSourceTimestamp(AdvanceSourceTimestamp),
    StatementReady(StatementReady),
    SinkConnectorReady(SinkConnectorReady),
    Shutdown,
}

pub struct AdvanceSourceTimestamp {
    pub id: SourceInstanceId,
    pub update: TimestampSourceUpdate,
}

pub struct StatementReady {
    pub session: Session,
    pub tx: ClientTransmitter<ExecuteResponse>,
    pub result: Result<sql::ast::Statement, anyhow::Error>,
    pub params: Params,
}

pub struct SinkConnectorReady {
    pub session: Session,
    pub tx: ClientTransmitter<ExecuteResponse>,
    pub id: GlobalId,
    pub oid: u32,
    pub result: Result<SinkConnector, anyhow::Error>,
}

#[derive(Clone, Debug)]
pub struct LoggingConfig {
    pub granularity: Duration,
    pub log_logging: bool,
}

pub struct Config<'a, C>
where
    C: comm::Connection,
{
    pub switchboard: comm::Switchboard<C>,
    pub cmd_rx: futures::channel::mpsc::UnboundedReceiver<Command>,
    pub num_timely_workers: usize,
    pub symbiosis_url: Option<&'a str>,
    pub logging: Option<LoggingConfig>,
    pub data_directory: &'a Path,
    pub timestamp: TimestampConfig,
    pub cache: Option<CacheConfig>,
    pub logical_compaction_window: Option<Duration>,
    pub experimental_mode: bool,
    pub build_info: &'static BuildInfo,
}

/// Glues the external world to the Timely workers.
pub struct Coordinator<C>
where
    C: comm::Connection,
{
    switchboard: comm::Switchboard<C>,
    broadcast_tx: comm::broadcast::Sender<SequencedCommand>,
    num_timely_workers: usize,
    optimizer: Optimizer,
    catalog: Catalog,
    symbiosis: Option<symbiosis::Postgres>,
    /// Maps (global Id of arrangement) -> (frontier information)
    indexes: ArrangementFrontiers<Timestamp>,
    since_updates: Vec<(GlobalId, Antichain<Timestamp>)>,
    /// For each connection running a TAIL command, the name of the dataflow
    /// that is servicing the TAIL. A connection can only run one TAIL at a
    /// time.
    active_tails: HashMap<u32, GlobalId>,
    timestamp_config: TimestampConfig,
    /// Delta from leading edge of an arrangement from which we allow compaction.
    logical_compaction_window_ms: Option<Timestamp>,
    /// Instance count: number of times sources have been instantiated in views. This is used
    /// to associate each new instance of a source with a unique instance id (iid)
    logging_granularity: Option<u64>,
    // Channel to communicate source status updates and shutdown notifications to the cacher
    // thread.
    cache_tx: Option<CacheSender>,
    /// The last timestamp we assigned to a read.
    read_lower_bound: Timestamp,
    /// The timestamp that all local inputs have been advanced up to.
    closed_up_to: Timestamp,
    /// Whether or not the most recent operation was a read.
    last_op_was_read: bool,
    /// Whether we need to advance local inputs (i.e., did someone observe a timestamp).
    /// TODO(justin): this is a hack, and does not work right with TAIL.
    need_advance: bool,
    transient_id_counter: u64,
}

impl<C> Coordinator<C>
where
    C: comm::Connection,
{
    /// Assign a timestamp for a read.
    fn get_read_ts(&mut self) -> Timestamp {
        let ts = self.get_ts();
        self.last_op_was_read = true;
        self.read_lower_bound = ts;
        ts
    }

    /// Assign a timestamp for a write. Writes following reads must ensure that they are assigned a
    /// strictly larger timestamp to ensure they are not visible to any real-time earlier reads.
    fn get_write_ts(&mut self) -> Timestamp {
        let ts = if self.last_op_was_read {
            self.last_op_was_read = false;
            cmp::max(self.get_ts(), self.read_lower_bound + 1)
        } else {
            self.get_ts()
        };
        self.read_lower_bound = cmp::max(ts, self.closed_up_to);
        self.read_lower_bound
    }

    /// Fetch a new timestamp.
    fn get_ts(&mut self) -> Timestamp {
        // Next time we have a chance, we will force all local inputs forward.
        self.need_advance = true;
        // This is a hack. In a perfect world we would represent time as having a "real" dimension
        // and a "coordinator" dimension so that clients always observed linearizability from
        // things the coordinator did without being related to the real dimension.
        let ts = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("failed to get millis since epoch")
            .as_millis()
            .try_into()
            .expect("current time did not fit into u64");

        if ts < self.read_lower_bound {
            self.read_lower_bound
        } else {
            ts
        }
    }

    /// Initializes coordinator state based on the contained catalog. Must be
    /// called after creating the coordinator and before calling the
    /// `Coordinator::serve` method.
    async fn bootstrap(&mut self, events: Vec<catalog::Event>) -> Result<(), anyhow::Error> {
        let items: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                catalog::Event::CreatedItem {
                    id,
                    oid,
                    name,
                    item,
                    ..
                } => Some((id, oid, name, item)),
                _ => None,
            })
            .collect();

        // Sources and indexes may be depended upon by other catalog items,
        // insert them first.
        for &(id, _, _, item) in &items {
            match item {
                //currently catalog item rebuild assumes that sinks and
                //indexes are always built individually and does not store information
                //about how it was built. If we start building multiple sinks and/or indexes
                //using a single dataflow, we have to make sure the rebuild process re-runs
                //the same multiple-build dataflow.
                CatalogItem::Source(source) => {
                    self.maybe_begin_caching(*id, &source.connector).await;
                }
                CatalogItem::Index(_) => {
                    if BUILTINS.logs().any(|log| log.index_id == *id) {
                        // Indexes on logging views are special, as they are
                        // already installed in the dataflow plane via
                        // `SequencedCommand::EnableLogging`. Just teach the
                        // coordinator of their existence, without creating a
                        // dataflow for the index.
                        //
                        // TODO(benesch): why is this hardcoded to 1000?
                        // Should it not be the same logical compaction window
                        // that everything else uses?
                        self.indexes
                            .insert(*id, Frontiers::new(self.num_timely_workers, Some(1_000)));
                    } else {
                        self.ship_dataflow(self.dataflow_builder().build_index_dataflow(*id))
                            .await;
                    }
                }
                _ => (), // Handled in next loop.
            }
        }

        for &(id, oid, name, item) in &items {
            match item {
                CatalogItem::Table(_) | CatalogItem::View(_) => (),
                CatalogItem::Sink(sink) => {
                    let builder = match &sink.connector {
                        SinkConnectorState::Pending(builder) => builder,
                        SinkConnectorState::Ready(_) => {
                            panic!("sink already initialized during catalog boot")
                        }
                    };
                    let connector = sink_connector::build(
                        builder.clone(),
                        sink.with_snapshot,
                        self.determine_frontier(sink.as_of, sink.from)?,
                        *id,
                    )
                    .await
                    .with_context(|| format!("recreating sink {}", name))?;
                    self.handle_sink_connector_ready(*id, *oid, connector).await;
                }
                _ => (), // Handled in prior loop.
            }
        }

        self.process_catalog_events(events).await?;

        // Announce primary and foreign key relationships.
        if self.logging_granularity.is_some() {
            for log in BUILTINS.logs() {
                let log_id = &log.id.to_string();
                self.update_catalog_view(
                    MZ_VIEW_KEYS.id,
                    log.variant.desc().typ().keys.iter().enumerate().flat_map(
                        move |(index, key)| {
                            key.iter().map(move |k| {
                                let row = Row::pack_slice(&[
                                    Datum::String(log_id),
                                    Datum::Int64(*k as i64),
                                    Datum::Int64(index as i64),
                                ]);
                                (row, 1)
                            })
                        },
                    ),
                )
                .await;

                self.update_catalog_view(
                    MZ_VIEW_FOREIGN_KEYS.id,
                    log.variant.foreign_keys().into_iter().enumerate().flat_map(
                        move |(index, (parent, pairs))| {
                            let parent_id = BUILTINS
                                .logs()
                                .find(|src| src.variant == parent)
                                .unwrap()
                                .id
                                .to_string();
                            pairs.into_iter().map(move |(c, p)| {
                                let row = Row::pack_slice(&[
                                    Datum::String(&log_id),
                                    Datum::Int64(c as i64),
                                    Datum::String(&parent_id),
                                    Datum::Int64(p as i64),
                                    Datum::Int64(index as i64),
                                ]);
                                (row, 1)
                            })
                        },
                    ),
                )
                .await;
            }
        }

        Ok(())
    }

    /// Serves the coordinator, receiving commands from users over `cmd_rx`
    /// and feedback from dataflow workers over `feedback_rx`.
    ///
    /// You must call `bootstrap` before calling this method.
    async fn serve(
        mut self,
        cmd_rx: futures::channel::mpsc::UnboundedReceiver<Command>,
        feedback_rx: comm::mpsc::Receiver<WorkerFeedbackWithMeta>,
    ) {
        let (internal_cmd_tx, internal_cmd_stream) = futures::channel::mpsc::unbounded();

        let cmd_stream = cmd_rx
            .map(Message::Command)
            .chain(stream::once(future::ready(Message::Shutdown)));

        let feedback_stream = feedback_rx.map(|r| match r {
            Ok(m) => Message::Worker(m),
            Err(e) => panic!("coordinator feedback receiver failed: {}", e),
        });

        let (ts_tx, ts_rx) = std::sync::mpsc::channel();
        let mut timestamper =
            Timestamper::new(&self.timestamp_config, internal_cmd_tx.clone(), ts_rx);
        let executor = Handle::current();
        let _timestamper_thread = thread::spawn(move || {
            let _executor_guard = executor.enter();
            timestamper.update()
        })
        .join_on_drop();

        let mut messages = ore::future::select_all_biased(vec![
            // Order matters here. We want to drain internal commands
            // (`internal_cmd_stream` and `feedback_stream`) before processing
            // external commands (`cmd_stream`).
            internal_cmd_stream.boxed(),
            feedback_stream.boxed(),
            cmd_stream.boxed(),
        ]);

        while let Some(msg) = messages.next().await {
            match msg {
                Message::Command(cmd) => self.message_command(cmd, &internal_cmd_tx).await,
                Message::Worker(worker) => self.message_worker(worker, &ts_tx).await,
                Message::StatementReady(ready) => {
                    self.message_statement_ready(ready, &internal_cmd_tx).await
                }
                Message::SinkConnectorReady(ready) => {
                    self.message_sink_connector_ready(ready).await
                }
                Message::AdvanceSourceTimestamp(advance) => {
                    self.message_advance_source_timestamp(advance).await
                }
                Message::Shutdown => {
                    self.message_shutdown(&ts_tx).await;
                    break;
                }
            }

            let needed = self.need_advance;
            let mut next_ts = self.get_ts();
            self.need_advance = false;
            if next_ts <= self.read_lower_bound {
                next_ts = self.read_lower_bound + 1;
            }
            // TODO(justin): this is pretty hacky, and works more-or-less because this frequency
            // lines up with that used in the logging views.
            if needed
                || self.logging_granularity.is_some()
                    && next_ts / self.logging_granularity.unwrap()
                        > self.closed_up_to / self.logging_granularity.unwrap()
            {
                if next_ts > self.closed_up_to {
                    broadcast(
                        &mut self.broadcast_tx,
                        SequencedCommand::AdvanceAllLocalInputs {
                            advance_to: next_ts,
                        },
                    )
                    .await;
                    self.closed_up_to = next_ts;
                }
            }
        }

        // Cleanly drain any pending messages from the worker before shutting
        // down.
        drop(internal_cmd_tx);
        while messages.next().await.is_some() {}
    }

    async fn message_worker(
        &mut self,
        WorkerFeedbackWithMeta {
            worker_id: _,
            message,
        }: WorkerFeedbackWithMeta,
        ts_tx: &std::sync::mpsc::Sender<TimestampMessage>,
    ) {
        match message {
            WorkerFeedback::FrontierUppers(updates) => {
                for (name, changes) in updates {
                    self.update_upper(&name, changes);
                }
                self.maintenance().await;
            }
            WorkerFeedback::DroppedSource(source_id) => {
                // Notify timestamping thread that source has been dropped
                ts_tx
                    .send(TimestampMessage::DropInstance(source_id))
                    .expect("Failed to send Drop Instance notice to timestamper");
            }
            WorkerFeedback::CreateSource(src_instance_id) => {
                if let Some(entry) = self.catalog.try_get_by_id(src_instance_id.source_id) {
                    if let CatalogItem::Source(s) = entry.item() {
                        ts_tx
                            .send(TimestampMessage::Add(src_instance_id, s.connector.clone()))
                            .expect("Failed to send CREATE Instance notice to timestamper");
                    } else {
                        panic!("A non-source is re-using the same source ID");
                    }
                } else {
                    // Someone already dropped the source
                }
            }
        }
    }

    async fn message_statement_ready(
        &mut self,
        StatementReady {
            session,
            tx,
            result,
            params,
        }: StatementReady,
        internal_cmd_tx: &futures::channel::mpsc::UnboundedSender<Message>,
    ) {
        match future::ready(result)
            .and_then(|stmt| self.handle_statement(&session, stmt, &params))
            .await
        {
            Ok((pcx, plan)) => {
                self.sequence_plan(&internal_cmd_tx, tx, session, pcx, plan)
                    .await
            }
            Err(e) => tx.send(Err(e), session),
        }
    }

    async fn message_sink_connector_ready(
        &mut self,
        SinkConnectorReady {
            session,
            tx,
            id,
            oid,
            result,
        }: SinkConnectorReady,
    ) {
        match result {
            Ok(connector) => {
                // NOTE: we must not fail from here on out. We have a
                // connector, which means there is external state (like
                // a Kafka topic) that's been created on our behalf. If
                // we fail now, we'll leak that external state.
                if self.catalog.try_get_by_id(id).is_some() {
                    self.handle_sink_connector_ready(id, oid, connector).await;
                } else {
                    // Another session dropped the sink while we were
                    // creating the connector. Report to the client that
                    // we created the sink, because from their
                    // perspective we did, as there is state (e.g. a
                    // Kafka topic) they need to clean up.
                }
                tx.send(Ok(ExecuteResponse::CreatedSink { existed: false }), session);
            }
            Err(e) => {
                self.catalog_transact(vec![catalog::Op::DropItem(id)])
                    .await
                    .expect("deleting placeholder sink cannot fail");
                tx.send(Err(e), session);
            }
        }
    }

    async fn message_shutdown(&mut self, ts_tx: &std::sync::mpsc::Sender<TimestampMessage>) {
        ts_tx.send(TimestampMessage::Shutdown).unwrap();

        if let Some(cache_tx) = &mut self.cache_tx {
            cache_tx
                .send(CacheMessage::Shutdown)
                .await
                .expect("failed to send shutdown message to caching thread");
        }
        broadcast(&mut self.broadcast_tx, SequencedCommand::Shutdown).await;
    }

    async fn message_advance_source_timestamp(
        &mut self,
        AdvanceSourceTimestamp { id, update }: AdvanceSourceTimestamp,
    ) {
        broadcast(
            &mut self.broadcast_tx,
            SequencedCommand::AdvanceSourceTimestamp { id, update },
        )
        .await;
    }

    async fn message_command(
        &mut self,
        cmd: Command,
        internal_cmd_tx: &futures::channel::mpsc::UnboundedSender<Message>,
    ) {
        match cmd {
            Command::Startup { session, tx } => {
                let mut messages = vec![];
                let catalog = self.catalog.for_session(&session);
                if catalog
                    .resolve_database(catalog.default_database())
                    .is_err()
                {
                    messages.push(StartupMessage::UnknownSessionDatabase);
                }
                if let Err(e) = self.catalog.create_temporary_schema(session.conn_id()) {
                    let _ = tx.send(Response {
                        result: Err(anyhow::Error::from(e)),
                        session,
                    });
                    return;
                }
                ClientTransmitter::new(tx).send(Ok(messages), session)
            }

            Command::Execute {
                portal_name,
                session,
                tx,
            } => {
                let result = session
                    .get_portal(&portal_name)
                    .ok_or_else(|| anyhow::format_err!("portal does not exist {:?}", portal_name));
                let portal = match result {
                    Ok(portal) => portal,
                    Err(e) => {
                        let _ = tx.send(Response {
                            result: Err(e),
                            session,
                        });
                        return;
                    }
                };
                match &portal.stmt {
                    Some(stmt) => {
                        let mut internal_cmd_tx = internal_cmd_tx.clone();
                        let stmt = stmt.clone();
                        let params = portal.parameters.clone();
                        tokio::spawn(async move {
                            let result = sql::pure::purify(stmt).await;
                            internal_cmd_tx
                                .send(Message::StatementReady(StatementReady {
                                    session,
                                    tx: ClientTransmitter::new(tx),
                                    result,
                                    params,
                                }))
                                .await
                                .expect("sending to internal_cmd_tx cannot fail");
                        });
                    }
                    None => {
                        let _ = tx.send(Response {
                            result: Ok(ExecuteResponse::EmptyQuery),
                            session,
                        });
                    }
                }
            }

            // NoSessionExecute is designed to support a limited set of queries that
            // run as the system user and are not associated with a user session. Due to
            // that limitation, they do not support all plans (some of which require side
            // effects in the session).
            Command::NoSessionExecute { stmt, params, tx } => {
                let res = async {
                    let stmt = sql::pure::purify(stmt).await?;
                    let catalog = self.catalog.for_system_session();
                    let desc = describe(&catalog, stmt.clone(), &[], None)?;
                    let pcx = PlanContext::default();
                    let plan = sql::plan::plan(&pcx, &catalog, stmt, &params)?;
                    // At time of writing this comment, Peeks use the connection id only for
                    // logging, so it is safe to reuse the system id, which is the conn_id from
                    // for_system_session().
                    let conn_id = catalog.conn_id();
                    let response = match plan {
                        Plan::Peek {
                            source,
                            when,
                            finishing,
                            copy_to,
                        } => {
                            self.sequence_peek(conn_id, source, when, finishing, copy_to)
                                .await?
                        }

                        Plan::SendRows(rows) => send_immediate_rows(rows),

                        _ => bail!("unsupported plan"),
                    };
                    Ok(NoSessionExecuteResponse {
                        desc: desc.relation_desc,
                        response,
                    })
                }
                .await;
                let _ = tx.send(res);
            }

            Command::Declare {
                name,
                stmt,
                param_types,
                mut session,
                tx,
            } => {
                let result = self.handle_declare(&mut session, name, stmt, param_types);
                let _ = tx.send(Response { result, session });
            }

            Command::Describe {
                name,
                stmt,
                param_types,
                mut session,
                tx,
            } => {
                let result = self.handle_describe(&mut session, name, stmt, param_types);
                let _ = tx.send(Response { result, session });
            }

            Command::CancelRequest { conn_id } => {
                self.handle_cancel(conn_id).await;
            }

            Command::DumpCatalog { tx } => {
                let _ = tx.send(self.catalog.dump());
            }

            Command::Terminate { mut session } => {
                self.handle_terminate(&mut session).await;
            }
        }
    }

    /// Updates the upper frontier of a named view.
    fn update_upper(&mut self, name: &GlobalId, mut changes: ChangeBatch<Timestamp>) {
        if let Some(index_state) = self.indexes.get_mut(name) {
            let changes: Vec<_> = index_state.upper.update_iter(changes.drain()).collect();
            if !changes.is_empty() {
                // Advance the compaction frontier to trail the new frontier.
                // If the compaction latency is `None` compaction messages are
                // not emitted, and the trace should be broadly useable.
                // TODO: If the frontier advances surprisingly quickly, e.g. in
                // the case of a constant collection, this compaction is actively
                // harmful. We should reconsider compaction policy with an eye
                // towards minimizing unexpected screw-ups.
                if let Some(compaction_window_ms) = index_state.compaction_window_ms {
                    // Decline to compact complete collections. This would have the
                    // effect of making the collection unusable. Instead, we would
                    // prefer to compact collections only when we believe it would
                    // reduce the volume of the collection, but we don't have that
                    // information here.
                    if !index_state.upper.frontier().is_empty() {
                        let mut compaction_frontier = Antichain::new();
                        for time in index_state.upper.frontier().iter() {
                            compaction_frontier.insert(
                                compaction_window_ms
                                    * (time.saturating_sub(compaction_window_ms)
                                        / compaction_window_ms),
                            );
                        }
                        if index_state.since != compaction_frontier {
                            index_state.advance_since(&compaction_frontier);
                            self.since_updates
                                .push((name.clone(), index_state.since.clone()));
                        }
                    }
                }
            }
        }
    }

    /// Perform maintenance work associated with the coordinator.
    ///
    /// Primarily, this involves sequencing compaction commands, which should be
    /// issued whenever available.
    async fn maintenance(&mut self) {
        // Take this opportunity to drain `since_update` commands.
        // Don't try to compact to an empty frontier. There may be a good reason to do this
        // in principle, but not in any current Mz use case.
        // (For background, see: https://github.com/MaterializeInc/materialize/pull/1113#issuecomment-559281990)
        self.since_updates
            .retain(|(_, frontier)| frontier != &Antichain::new());
        if !self.since_updates.is_empty() {
            broadcast(
                &mut self.broadcast_tx,
                SequencedCommand::AllowCompaction(std::mem::replace(
                    &mut self.since_updates,
                    Vec::new(),
                )),
            )
            .await;
        }
    }

    async fn handle_statement(
        &mut self,
        session: &Session,
        stmt: sql::ast::Statement,
        params: &sql::plan::Params,
    ) -> Result<(PlanContext, sql::plan::Plan), anyhow::Error> {
        let pcx = PlanContext::default();

        // When symbiosis mode is enabled, use symbiosis planning for:
        //  - CREATE TABLE
        //  - DROP TABLE
        //  - INSERT
        // When these statements are routed through symbiosis, table information
        // is created and maintained locally, which is required for other statements
        // to be executed correctly.
        if let Statement::CreateTable(CreateTableStatement { .. })
        | Statement::DropObjects(DropObjectsStatement {
            object_type: ObjectType::Table,
            ..
        })
        | Statement::Insert { .. } = &stmt
        {
            if let Some(ref mut postgres) = self.symbiosis {
                let plan = postgres
                    .execute(&pcx, &self.catalog.for_session(session), &stmt)
                    .await?;
                return Ok((pcx, plan));
            }
        }

        match sql::plan::plan(
            &pcx,
            &self.catalog.for_session(session),
            stmt.clone(),
            params,
        ) {
            Ok(plan) => Ok((pcx, plan)),
            Err(err) => match self.symbiosis {
                Some(ref mut postgres) if postgres.can_handle(&stmt) => {
                    let plan = postgres
                        .execute(&pcx, &self.catalog.for_session(session), &stmt)
                        .await?;
                    Ok((pcx, plan))
                }
                _ => Err(err),
            },
        }
    }

    fn handle_declare(
        &self,
        session: &mut Session,
        name: String,
        stmt: Statement,
        param_types: Vec<Option<pgrepr::Type>>,
    ) -> Result<(), anyhow::Error> {
        // handle_describe cares about symbiosis mode here. Declared cursors are
        // perhaps rare enough we can ignore that worry and just error instead.
        let desc = describe(
            &self.catalog.for_session(session),
            stmt.clone(),
            &param_types,
            Some(session),
        )?;
        let params = vec![];
        let result_formats = vec![pgrepr::Format::Text; desc.arity()];
        session.set_portal(name, desc, Some(stmt), params, result_formats);
        Ok(())
    }

    fn handle_describe(
        &self,
        session: &mut Session,
        name: String,
        stmt: Option<Statement>,
        param_types: Vec<Option<pgrepr::Type>>,
    ) -> Result<(), anyhow::Error> {
        let desc = if let Some(stmt) = stmt.clone() {
            match describe(
                &self.catalog.for_session(session),
                stmt.clone(),
                &param_types,
                Some(session),
            ) {
                Ok(desc) => desc,
                // Describing the query failed. If we're running in symbiosis with
                // Postgres, see if Postgres can handle it. Note that Postgres
                // only handles commands that do not return rows, so the
                // `StatementDesc` is constructed accordingly.
                Err(err) => match self.symbiosis {
                    Some(ref postgres) if postgres.can_handle(&stmt) => StatementDesc::new(None),
                    _ => return Err(err),
                },
            }
        } else {
            StatementDesc::new(None)
        };
        session.set_prepared_statement(name, PreparedStatement::new(stmt, desc));
        Ok(())
    }

    /// Instruct the dataflow layer to cancel any ongoing, interactive work for
    /// the named `conn_id`. This means canceling the active PEEK or TAIL, if
    /// one exists.
    ///
    /// NOTE(benesch): this function makes the assumption that a connection can
    /// only have one active query at a time. This is true today, but will not
    /// be true once we have full support for portals.
    async fn handle_cancel(&mut self, conn_id: u32) {
        if let Some(name) = self.active_tails.remove(&conn_id) {
            // A TAIL is known to be active, so drop the dataflow that is
            // servicing it. No need to try to cancel PEEKs in this case,
            // because if a TAIL is active, a PEEK cannot be.
            self.drop_sinks(vec![name]).await;
        } else {
            // No TAIL is known to be active, so drop the PEEK that may be
            // active on this connection. This is a no-op if no PEEKs are
            // active.
            broadcast(
                &mut self.broadcast_tx,
                SequencedCommand::CancelPeek { conn_id },
            )
            .await;
        }
    }

    /// Handle termination of a client session.
    ///
    // This cleans up any state in the coordinator associated with the session.
    async fn handle_terminate(&mut self, session: &mut Session) {
        if let Some(name) = self.active_tails.remove(&session.conn_id()) {
            self.drop_sinks(vec![name]).await;
        }
        self.drop_temp_items(session.conn_id()).await;
        self.catalog
            .drop_temporary_schema(session.conn_id())
            .expect("unable to drop temporary schema");
    }

    // Removes all temporary items created by the specified connection, though
    // not the temporary schema itself.
    async fn drop_temp_items(&mut self, conn_id: u32) {
        let ops = self.catalog.drop_temp_item_ops(conn_id);
        self.catalog_transact(ops)
            .await
            .expect("unable to drop temporary items for conn_id");
    }

    async fn handle_sink_connector_ready(
        &mut self,
        id: GlobalId,
        oid: u32,
        connector: SinkConnector,
    ) {
        // Update catalog entry with sink connector.
        let entry = self.catalog.get_by_id(&id);
        let name = entry.name().clone();
        let mut sink = match entry.item() {
            CatalogItem::Sink(sink) => sink.clone(),
            _ => unreachable!(),
        };
        sink.connector = catalog::SinkConnectorState::Ready(connector.clone());
        let ops = vec![
            catalog::Op::DropItem(id),
            catalog::Op::CreateItem {
                id,
                oid,
                name: name.clone(),
                item: CatalogItem::Sink(sink.clone()),
            },
        ];
        self.catalog_transact(ops)
            .await
            .expect("replacing a sink cannot fail");

        self.ship_dataflow(self.dataflow_builder().build_sink_dataflow(
            name.to_string(),
            id,
            sink.from,
            connector,
        ))
        .await
    }

    /// Insert a single row into a given catalog view.
    async fn update_catalog_view<I>(&mut self, index_id: GlobalId, updates: I)
    where
        I: IntoIterator<Item = (Row, isize)>,
    {
        let timestamp = self.get_write_ts();
        let updates = updates
            .into_iter()
            .map(|(row, diff)| Update {
                row,
                diff,
                timestamp,
            })
            .collect();
        broadcast(
            &mut self.broadcast_tx,
            SequencedCommand::Insert {
                id: index_id,
                updates,
            },
        )
        .await;
    }

    async fn report_database_update(
        &mut self,
        database_id: i64,
        oid: u32,
        name: &str,
        diff: isize,
    ) {
        self.update_catalog_view(
            MZ_DATABASES.id,
            iter::once((
                Row::pack_slice(&[
                    Datum::Int64(database_id),
                    Datum::Int32(oid as i32),
                    Datum::String(&name),
                ]),
                diff,
            )),
        )
        .await
    }

    async fn report_schema_update(
        &mut self,
        schema_id: i64,
        oid: u32,
        database_id: Option<i64>,
        schema_name: &str,
        diff: isize,
    ) {
        self.update_catalog_view(
            MZ_SCHEMAS.id,
            iter::once((
                Row::pack_slice(&[
                    Datum::Int64(schema_id),
                    Datum::Int32(oid as i32),
                    match database_id {
                        None => Datum::Null,
                        Some(database_id) => Datum::Int64(database_id),
                    },
                    Datum::String(schema_name),
                ]),
                diff,
            )),
        )
        .await
    }

    async fn report_column_updates(
        &mut self,
        desc: &RelationDesc,
        global_id: GlobalId,
        diff: isize,
    ) -> Result<(), anyhow::Error> {
        for (i, (column_name, column_type)) in desc.iter().enumerate() {
            self.update_catalog_view(
                MZ_COLUMNS.id,
                iter::once((
                    Row::pack_slice(&[
                        Datum::String(&global_id.to_string()),
                        Datum::String(
                            &column_name
                                .map(|n| n.to_string())
                                .unwrap_or_else(|| "?column?".to_owned()),
                        ),
                        Datum::Int64(i as i64 + 1),
                        Datum::from(column_type.nullable),
                        Datum::String(pgrepr::Type::from(&column_type.scalar_type).name()),
                    ]),
                    diff,
                )),
            )
            .await
        }
        Ok(())
    }

    async fn report_index_update(
        &mut self,
        global_id: GlobalId,
        oid: u32,
        index: &Index,
        name: &str,
        diff: isize,
    ) {
        self.report_index_update_inner(
            global_id,
            oid,
            index,
            name,
            index
                .keys
                .iter()
                .map(|key| {
                    key.typ(self.catalog.get_by_id(&index.on).desc().unwrap().typ())
                        .nullable
                })
                .collect(),
            diff,
        )
        .await
    }

    // When updating the mz_indexes system table after dropping an index, it may no longer be possible
    // to generate the 'nullable' information for that index. This function allows callers to bypass
    // that computation and provide their own value, instead.
    async fn report_index_update_inner(
        &mut self,
        global_id: GlobalId,
        oid: u32,
        index: &Index,
        name: &str,
        nullable: Vec<bool>,
        diff: isize,
    ) {
        let key_sqls = match sql::parse::parse(&index.create_sql)
            .expect("create_sql cannot be invalid")
            .into_element()
        {
            Statement::CreateIndex(CreateIndexStatement { key_parts, .. }) => key_parts.unwrap(),
            _ => unreachable!(),
        };
        self.update_catalog_view(
            MZ_INDEXES.id,
            iter::once((
                Row::pack_slice(&[
                    Datum::String(&global_id.to_string()),
                    Datum::Int32(oid as i32),
                    Datum::String(name),
                    Datum::String(&index.on.to_string()),
                ]),
                diff,
            )),
        )
        .await;

        for (i, key) in index.keys.iter().enumerate() {
            let nullable = *nullable
                .get(i)
                .expect("missing nullability information for index key");
            let seq_in_index = i64::try_from(i + 1).expect("invalid index sequence number");
            let key_sql = key_sqls
                .get(i)
                .expect("missing sql information for index key")
                .to_string();
            let (field_number, expression) = match key {
                ScalarExpr::Column(col) => (
                    Datum::Int64(i64::try_from(*col + 1).expect("invalid index column number")),
                    Datum::Null,
                ),
                _ => (Datum::Null, Datum::String(&key_sql)),
            };
            self.update_catalog_view(
                MZ_INDEX_COLUMNS.id,
                iter::once((
                    Row::pack_slice(&[
                        Datum::String(&global_id.to_string()),
                        Datum::Int64(seq_in_index),
                        field_number,
                        expression,
                        Datum::from(nullable),
                    ]),
                    diff,
                )),
            )
            .await
        }
    }

    async fn report_table_update(
        &mut self,
        global_id: GlobalId,
        oid: u32,
        schema_id: i64,
        name: &str,
        diff: isize,
    ) {
        self.update_catalog_view(
            MZ_TABLES.id,
            iter::once((
                Row::pack_slice(&[
                    Datum::String(&global_id.to_string()),
                    Datum::Int32(oid as i32),
                    Datum::Int64(schema_id),
                    Datum::String(name),
                ]),
                diff,
            )),
        )
        .await
    }

    async fn report_source_update(
        &mut self,
        global_id: GlobalId,
        oid: u32,
        schema_id: i64,
        name: &str,
        diff: isize,
    ) {
        self.update_catalog_view(
            MZ_SOURCES.id,
            iter::once((
                Row::pack_slice(&[
                    Datum::String(&global_id.to_string()),
                    Datum::Int32(oid as i32),
                    Datum::Int64(schema_id),
                    Datum::String(name),
                ]),
                diff,
            )),
        )
        .await
    }

    async fn report_view_update(
        &mut self,
        global_id: GlobalId,
        oid: u32,
        schema_id: i64,
        name: &str,
        diff: isize,
    ) {
        self.update_catalog_view(
            MZ_VIEWS.id,
            iter::once((
                Row::pack_slice(&[
                    Datum::String(&global_id.to_string()),
                    Datum::Int32(oid as i32),
                    Datum::Int64(schema_id),
                    Datum::String(name),
                ]),
                diff,
            )),
        )
        .await
    }

    async fn report_sink_update(
        &mut self,
        global_id: GlobalId,
        oid: u32,
        schema_id: i64,
        name: &str,
        diff: isize,
    ) {
        self.update_catalog_view(
            MZ_SINKS.id,
            iter::once((
                Row::pack_slice(&[
                    Datum::String(&global_id.to_string()),
                    Datum::Int32(oid as i32),
                    Datum::Int64(schema_id),
                    Datum::String(name),
                ]),
                diff,
            )),
        )
        .await
    }

    async fn report_type_update(
        &mut self,
        id: GlobalId,
        oid: u32,
        schema_id: i64,
        name: &str,
        typ: &Type,
        diff: isize,
    ) {
        self.update_catalog_view(
            MZ_TYPES.id,
            iter::once((
                Row::pack_slice(&[
                    Datum::String(&id.to_string()),
                    Datum::Int32(oid as i32),
                    Datum::Int64(schema_id),
                    Datum::String(name),
                ]),
                diff,
            )),
        )
        .await;

        let (index_id, update) = match typ.inner {
            TypeInner::Array { element_id } => (
                MZ_ARRAY_TYPES.id,
                vec![id.to_string(), element_id.to_string()],
            ),
            TypeInner::Base => (MZ_BASE_TYPES.id, vec![id.to_string()]),
            TypeInner::List { element_id } => (
                MZ_LIST_TYPES.id,
                vec![id.to_string(), element_id.to_string()],
            ),
            TypeInner::Map { key_id, value_id } => (
                MZ_MAP_TYPES.id,
                vec![id.to_string(), key_id.to_string(), value_id.to_string()],
            ),
        };
        self.update_catalog_view(
            index_id,
            iter::once((
                Row::pack_slice(&update.iter().map(|c| Datum::String(c)).collect::<Vec<_>>()[..]),
                diff,
            )),
        )
        .await
    }

    async fn sequence_plan(
        &mut self,
        internal_cmd_tx: &futures::channel::mpsc::UnboundedSender<Message>,
        tx: ClientTransmitter<ExecuteResponse>,
        mut session: Session,
        pcx: PlanContext,
        plan: Plan,
    ) {
        match plan {
            Plan::CreateDatabase {
                name,
                if_not_exists,
            } => tx.send(
                self.sequence_create_database(name, if_not_exists).await,
                session,
            ),

            Plan::CreateSchema {
                database_name,
                schema_name,
                if_not_exists,
            } => tx.send(
                self.sequence_create_schema(database_name, schema_name, if_not_exists)
                    .await,
                session,
            ),

            Plan::CreateTable {
                name,
                table,
                if_not_exists,
            } => tx.send(
                self.sequence_create_table(pcx, name, table, if_not_exists)
                    .await,
                session,
            ),

            Plan::CreateSource {
                name,
                source,
                if_not_exists,
                materialized,
            } => tx.send(
                self.sequence_create_source(pcx, name, source, if_not_exists, materialized)
                    .await,
                session,
            ),

            Plan::CreateSink {
                name,
                sink,
                with_snapshot,
                as_of,
                if_not_exists,
            } => {
                self.sequence_create_sink(
                    pcx,
                    internal_cmd_tx.clone(),
                    tx,
                    session,
                    name,
                    sink,
                    with_snapshot,
                    as_of,
                    if_not_exists,
                )
                .await
            }

            Plan::CreateView {
                name,
                view,
                replace,
                materialize,
                if_not_exists,
            } => tx.send(
                self.sequence_create_view(
                    pcx,
                    name,
                    view,
                    replace,
                    session.conn_id(),
                    materialize,
                    if_not_exists,
                )
                .await,
                session,
            ),

            Plan::CreateIndex {
                name,
                index,
                if_not_exists,
            } => tx.send(
                self.sequence_create_index(pcx, name, index, if_not_exists)
                    .await,
                session,
            ),

            Plan::CreateType { name, typ } => {
                tx.send(self.sequence_create_type(pcx, name, typ).await, session)
            }

            Plan::DropDatabase { name } => {
                tx.send(self.sequence_drop_database(name).await, session)
            }

            Plan::DropSchema { name } => tx.send(self.sequence_drop_schema(name).await, session),

            Plan::DropItems { items, ty } => {
                tx.send(self.sequence_drop_items(items, ty).await, session)
            }

            Plan::EmptyQuery => tx.send(Ok(ExecuteResponse::EmptyQuery), session),

            Plan::ShowAllVariables => {
                tx.send(self.sequence_show_all_variables(&session).await, session)
            }

            Plan::ShowVariable(name) => {
                tx.send(self.sequence_show_variable(&session, name).await, session)
            }

            Plan::SetVariable { name, value } => tx.send(
                self.sequence_set_variable(&mut session, name, value).await,
                session,
            ),

            Plan::StartTransaction => {
                session.start_transaction();
                tx.send(Ok(ExecuteResponse::StartedTransaction), session)
            }

            Plan::CommitTransaction | Plan::AbortTransaction => {
                let was_implicit = matches!(
                    session.transaction(),
                    TransactionStatus::InTransactionImplicit
                );
                let tag = match plan {
                    Plan::CommitTransaction => "COMMIT",
                    Plan::AbortTransaction => "ROLLBACK",
                    _ => unreachable!(),
                }
                .to_string();
                session.end_transaction();
                tx.send(
                    Ok(ExecuteResponse::TransactionExited { tag, was_implicit }),
                    session,
                )
            }

            Plan::Peek {
                source,
                when,
                finishing,
                copy_to,
            } => tx.send(
                self.sequence_peek(session.conn_id(), source, when, finishing, copy_to)
                    .await,
                session,
            ),

            Plan::Tail {
                id,
                ts,
                with_snapshot,
                copy_to,
                emit_progress,
                object_columns,
            } => tx.send(
                self.sequence_tail(
                    &session,
                    id,
                    with_snapshot,
                    ts,
                    copy_to,
                    emit_progress,
                    object_columns,
                )
                .await,
                session,
            ),

            Plan::SendRows(rows) => tx.send(Ok(send_immediate_rows(rows)), session),

            Plan::ExplainPlan {
                raw_plan,
                decorrelated_plan,
                row_set_finishing,
                stage,
                options,
            } => tx.send(
                self.sequence_explain_plan(
                    &session,
                    raw_plan,
                    decorrelated_plan,
                    row_set_finishing,
                    stage,
                    options,
                ),
                session,
            ),

            Plan::SendDiffs {
                id,
                updates,
                affected_rows,
                kind,
            } => tx.send(
                self.sequence_send_diffs(id, updates, affected_rows, kind)
                    .await,
                session,
            ),

            Plan::Insert { id, values } => tx.send(self.sequence_insert(id, values).await, session),

            Plan::AlterItemRename {
                id,
                to_name,
                object_type,
            } => tx.send(
                self.sequence_alter_item_rename(id, to_name, object_type)
                    .await,
                session,
            ),

            Plan::AlterIndexLogicalCompactionWindow(alter_index) => tx.send(
                self.sequence_alter_index_logical_compaction_window(alter_index),
                session,
            ),

            Plan::DiscardTemp => {
                self.drop_temp_items(session.conn_id()).await;
                tx.send(Ok(ExecuteResponse::DiscardedTemp), session);
            }

            Plan::DiscardAll => {
                let ret = if session.transaction() != &TransactionStatus::Idle {
                    ExecuteResponse::PgError {
                        code: SqlState::ACTIVE_SQL_TRANSACTION,
                        message: "DISCARD ALL cannot run inside a transaction block".to_string(),
                    }
                } else {
                    self.drop_temp_items(session.conn_id()).await;
                    session.reset();
                    ExecuteResponse::DiscardedAll
                };
                tx.send(Ok(ret), session);
            }

            Plan::Declare { name, stmt } => {
                let param_types = vec![];
                let res = self
                    .handle_declare(&mut session, name, stmt, param_types)
                    .map(|()| ExecuteResponse::DeclaredCursor);
                tx.send(res, session);
            }

            Plan::Fetch {
                name,
                count,
                timeout,
            } => tx.send(
                Ok(ExecuteResponse::Fetch {
                    name,
                    count,
                    timeout,
                }),
                session,
            ),

            Plan::Close { name } => {
                if session.remove_portal(&name) {
                    tx.send(Ok(ExecuteResponse::ClosedCursor), session)
                } else {
                    tx.send(Err(anyhow!("cursor \"{}\" does not exist", name)), session)
                }
            }
        }
    }

    async fn sequence_create_database(
        &mut self,
        name: String,
        if_not_exists: bool,
    ) -> Result<ExecuteResponse, anyhow::Error> {
        let db_oid = self.catalog.allocate_oid()?;
        let schema_oid = self.catalog.allocate_oid()?;
        let ops = vec![
            catalog::Op::CreateDatabase {
                name: name.clone(),
                oid: db_oid,
            },
            catalog::Op::CreateSchema {
                database_name: DatabaseSpecifier::Name(name),
                schema_name: "public".into(),
                oid: schema_oid,
            },
        ];
        match self.catalog_transact(ops).await {
            Ok(_) => Ok(ExecuteResponse::CreatedDatabase { existed: false }),
            Err(_) if if_not_exists => Ok(ExecuteResponse::CreatedDatabase { existed: true }),
            Err(err) => Err(err),
        }
    }

    async fn sequence_create_schema(
        &mut self,
        database_name: DatabaseSpecifier,
        schema_name: String,
        if_not_exists: bool,
    ) -> Result<ExecuteResponse, anyhow::Error> {
        let oid = self.catalog.allocate_oid()?;
        let op = catalog::Op::CreateSchema {
            database_name,
            schema_name,
            oid,
        };
        match self.catalog_transact(vec![op]).await {
            Ok(_) => Ok(ExecuteResponse::CreatedSchema { existed: false }),
            Err(_) if if_not_exists => Ok(ExecuteResponse::CreatedSchema { existed: true }),
            Err(err) => Err(err),
        }
    }

    async fn sequence_create_table(
        &mut self,
        pcx: PlanContext,
        name: FullName,
        table: sql::plan::Table,
        if_not_exists: bool,
    ) -> Result<ExecuteResponse, anyhow::Error> {
        let table_id = self.catalog.allocate_id()?;
        let table = catalog::Table {
            create_sql: table.create_sql,
            plan_cx: pcx,
            desc: table.desc,
            defaults: table.defaults,
        };
        let index_id = self.catalog.allocate_id()?;
        let mut index_name = name.clone();
        index_name.item += "_primary_idx";
        let index =
            auto_generate_primary_idx(index_name.item.clone(), name.clone(), table_id, &table.desc);
        let table_oid = self.catalog.allocate_oid()?;
        let index_oid = self.catalog.allocate_oid()?;
        match self
            .catalog_transact(vec![
                catalog::Op::CreateItem {
                    id: table_id,
                    oid: table_oid,
                    name,
                    item: CatalogItem::Table(table),
                },
                catalog::Op::CreateItem {
                    id: index_id,
                    oid: index_oid,
                    name: index_name,
                    item: CatalogItem::Index(index),
                },
            ])
            .await
        {
            Ok(_) => {
                self.ship_dataflow(self.dataflow_builder().build_index_dataflow(index_id))
                    .await;
                Ok(ExecuteResponse::CreatedTable { existed: false })
            }
            Err(_) if if_not_exists => Ok(ExecuteResponse::CreatedTable { existed: true }),
            Err(err) => Err(err),
        }
    }

    async fn sequence_create_source(
        &mut self,
        pcx: PlanContext,
        name: FullName,
        source: sql::plan::Source,
        if_not_exists: bool,
        materialized: bool,
    ) -> Result<ExecuteResponse, anyhow::Error> {
        let source = catalog::Source {
            create_sql: source.create_sql,
            plan_cx: pcx,
            connector: source.connector,
            desc: source.desc,
        };
        let source_id = self.catalog.allocate_id()?;
        let source_oid = self.catalog.allocate_oid()?;
        let mut ops = vec![catalog::Op::CreateItem {
            id: source_id,
            oid: source_oid,
            name: name.clone(),
            item: CatalogItem::Source(source.clone()),
        }];
        let index_id = if materialized {
            let mut index_name = name.clone();
            index_name.item += "_primary_idx";
            let index =
                auto_generate_primary_idx(index_name.item.clone(), name, source_id, &source.desc);
            let index_id = self.catalog.allocate_id()?;
            let index_oid = self.catalog.allocate_oid()?;
            ops.push(catalog::Op::CreateItem {
                id: index_id,
                oid: index_oid,
                name: index_name,
                item: CatalogItem::Index(index),
            });
            Some(index_id)
        } else {
            None
        };
        match self.catalog_transact(ops).await {
            Ok(()) => {
                if let Some(index_id) = index_id {
                    self.ship_dataflow(self.dataflow_builder().build_index_dataflow(index_id))
                        .await;
                }

                self.maybe_begin_caching(source_id, &source.connector).await;
                Ok(ExecuteResponse::CreatedSource { existed: false })
            }
            Err(_) if if_not_exists => Ok(ExecuteResponse::CreatedSource { existed: true }),
            Err(err) => Err(err),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn sequence_create_sink(
        &mut self,
        pcx: PlanContext,
        mut internal_cmd_tx: futures::channel::mpsc::UnboundedSender<Message>,
        tx: ClientTransmitter<ExecuteResponse>,
        session: Session,
        name: FullName,
        sink: sql::plan::Sink,
        with_snapshot: bool,
        as_of: Option<u64>,
        if_not_exists: bool,
    ) {
        // First try to allocate an ID and an OID. If either fails, we're done.
        let id = match self.catalog.allocate_id() {
            Ok(id) => id,
            Err(e) => {
                tx.send(Err(e.into()), session);
                return;
            }
        };
        let oid = match self.catalog.allocate_oid() {
            Ok(id) => id,
            Err(e) => {
                tx.send(Err(e.into()), session);
                return;
            }
        };

        let frontier = match self.determine_frontier(as_of, sink.from) {
            Ok(frontier) => frontier,
            Err(e) => {
                tx.send(Err(e), session);
                return;
            }
        };

        // Then try to create a placeholder catalog item with an unknown
        // connector. If that fails, we're done, though if the client specified
        // `if_not_exists` we'll tell the client we succeeded.
        //
        // This placeholder catalog item reserves the name while we create
        // the sink connector, which could take an arbitrarily long time.
        let op = catalog::Op::CreateItem {
            id,
            oid,
            name,
            item: CatalogItem::Sink(catalog::Sink {
                create_sql: sink.create_sql,
                plan_cx: pcx,
                from: sink.from,
                connector: catalog::SinkConnectorState::Pending(sink.connector_builder.clone()),
                with_snapshot,
                as_of,
            }),
        };
        match self.catalog_transact(vec![op]).await {
            Ok(()) => (),
            Err(_) if if_not_exists => {
                tx.send(Ok(ExecuteResponse::CreatedSink { existed: true }), session);
                return;
            }
            Err(e) => {
                tx.send(Err(e), session);
                return;
            }
        }

        // Now we're ready to create the sink connector. Arrange to notify the
        // main coordinator thread when the future completes.
        let connector_builder = sink.connector_builder;
        tokio::spawn(async move {
            internal_cmd_tx
                .send(Message::SinkConnectorReady(SinkConnectorReady {
                    session,
                    tx,
                    id,
                    oid,
                    result: sink_connector::build(connector_builder, with_snapshot, frontier, id)
                        .await,
                }))
                .await
                .expect("sending to internal_cmd_tx cannot fail");
        });
    }

    #[allow(clippy::too_many_arguments)]
    async fn sequence_create_view(
        &mut self,
        pcx: PlanContext,
        name: FullName,
        view: sql::plan::View,
        replace: Option<GlobalId>,
        conn_id: u32,
        materialize: bool,
        if_not_exists: bool,
    ) -> Result<ExecuteResponse, anyhow::Error> {
        let mut ops = vec![];
        if let Some(id) = replace {
            ops.extend(self.catalog.drop_items_ops(&[id]));
        }
        let view_id = self.catalog.allocate_id()?;
        let view_oid = self.catalog.allocate_oid()?;
        // Optimize the expression so that we can form an accurately typed description.
        let optimized_expr = self.prep_relation_expr(view.expr, ExprPrepStyle::Static)?;
        let desc = RelationDesc::new(optimized_expr.as_ref().typ(), view.column_names);
        let view = catalog::View {
            create_sql: view.create_sql,
            plan_cx: pcx,
            optimized_expr,
            desc,
            conn_id: if view.temporary { Some(conn_id) } else { None },
        };
        ops.push(catalog::Op::CreateItem {
            id: view_id,
            oid: view_oid,
            name: name.clone(),
            item: CatalogItem::View(view.clone()),
        });
        let index_id = if materialize {
            let mut index_name = name.clone();
            index_name.item += "_primary_idx";
            let index =
                auto_generate_primary_idx(index_name.item.clone(), name, view_id, &view.desc);
            let index_id = self.catalog.allocate_id()?;
            let index_oid = self.catalog.allocate_oid()?;
            ops.push(catalog::Op::CreateItem {
                id: index_id,
                oid: index_oid,
                name: index_name,
                item: CatalogItem::Index(index),
            });
            Some(index_id)
        } else {
            None
        };
        match self.catalog_transact(ops).await {
            Ok(()) => {
                if let Some(index_id) = index_id {
                    self.ship_dataflow(self.dataflow_builder().build_index_dataflow(index_id))
                        .await;
                }
                Ok(ExecuteResponse::CreatedView { existed: false })
            }
            Err(_) if if_not_exists => Ok(ExecuteResponse::CreatedView { existed: true }),
            Err(err) => Err(err),
        }
    }

    async fn sequence_create_index(
        &mut self,
        pcx: PlanContext,
        name: FullName,
        mut index: sql::plan::Index,
        if_not_exists: bool,
    ) -> Result<ExecuteResponse, anyhow::Error> {
        for key in &mut index.keys {
            Self::prep_scalar_expr(key, ExprPrepStyle::Static)?;
        }
        let index = catalog::Index {
            create_sql: index.create_sql,
            plan_cx: pcx,
            keys: index.keys,
            on: index.on,
        };
        let id = self.catalog.allocate_id()?;
        let oid = self.catalog.allocate_oid()?;
        let op = catalog::Op::CreateItem {
            id,
            oid,
            name,
            item: CatalogItem::Index(index),
        };
        match self.catalog_transact(vec![op]).await {
            Ok(()) => {
                self.ship_dataflow(self.dataflow_builder().build_index_dataflow(id))
                    .await;
                Ok(ExecuteResponse::CreatedIndex { existed: false })
            }
            Err(_) if if_not_exists => Ok(ExecuteResponse::CreatedIndex { existed: true }),
            Err(err) => Err(err),
        }
    }

    async fn sequence_create_type(
        &mut self,
        pcx: PlanContext,
        name: FullName,
        typ: sql::plan::Type,
    ) -> Result<ExecuteResponse, anyhow::Error> {
        let typ = catalog::Type {
            create_sql: typ.create_sql,
            plan_cx: pcx,
            inner: typ.inner.into(),
        };
        let id = self.catalog.allocate_id()?;
        let oid = self.catalog.allocate_oid()?;
        let op = catalog::Op::CreateItem {
            id,
            oid,
            name,
            item: CatalogItem::Type(typ),
        };
        match self.catalog_transact(vec![op]).await {
            Ok(()) => Ok(ExecuteResponse::CreatedType),
            Err(err) => Err(err),
        }
    }

    async fn sequence_drop_database(
        &mut self,
        name: String,
    ) -> Result<ExecuteResponse, anyhow::Error> {
        let ops = self.catalog.drop_database_ops(name);
        self.catalog_transact(ops).await?;
        Ok(ExecuteResponse::DroppedDatabase)
    }

    async fn sequence_drop_schema(
        &mut self,
        name: SchemaName,
    ) -> Result<ExecuteResponse, anyhow::Error> {
        let ops = self.catalog.drop_schema_ops(name);
        self.catalog_transact(ops).await?;
        Ok(ExecuteResponse::DroppedSchema)
    }

    async fn sequence_drop_items(
        &mut self,
        items: Vec<GlobalId>,
        ty: ObjectType,
    ) -> Result<ExecuteResponse, anyhow::Error> {
        let ops = self.catalog.drop_items_ops(&items);
        self.catalog_transact(ops).await?;
        Ok(match ty {
            ObjectType::Schema => unreachable!(),
            ObjectType::Source => {
                for id in items.iter() {
                    if let Some(cache_tx) = &mut self.cache_tx {
                        cache_tx
                            .send(CacheMessage::DropSource(*id))
                            .await
                            .expect("failed to send DROP SOURCE to cache thread");
                    }
                }
                ExecuteResponse::DroppedSource
            }
            ObjectType::View => ExecuteResponse::DroppedView,
            ObjectType::Table => ExecuteResponse::DroppedTable,
            ObjectType::Sink => ExecuteResponse::DroppedSink,
            ObjectType::Index => ExecuteResponse::DroppedIndex,
            ObjectType::Type => ExecuteResponse::DroppedType,
            ObjectType::Object => unreachable!("generic OBJECT cannot be dropped"),
        })
    }

    async fn sequence_show_all_variables(
        &mut self,
        session: &Session,
    ) -> Result<ExecuteResponse, anyhow::Error> {
        let mut row_packer = RowPacker::new();
        Ok(send_immediate_rows(
            session
                .vars()
                .iter()
                .map(|v| {
                    row_packer.pack(&[
                        Datum::String(v.name()),
                        Datum::String(&v.value()),
                        Datum::String(v.description()),
                    ])
                })
                .collect(),
        ))
    }

    async fn sequence_show_variable(
        &self,
        session: &Session,
        name: String,
    ) -> Result<ExecuteResponse, anyhow::Error> {
        let variable = session.vars().get(&name)?;
        let row = Row::pack_slice(&[Datum::String(&variable.value())]);
        Ok(send_immediate_rows(vec![row]))
    }

    async fn sequence_set_variable(
        &self,
        session: &mut Session,
        name: String,
        value: String,
    ) -> Result<ExecuteResponse, anyhow::Error> {
        session.vars_mut().set(&name, &value)?;
        Ok(ExecuteResponse::SetVariable { name })
    }

    async fn sequence_peek(
        &mut self,
        conn_id: u32,
        source: RelationExpr,
        when: PeekWhen,
        finishing: RowSetFinishing,
        copy_to: Option<CopyFormat>,
    ) -> Result<ExecuteResponse, anyhow::Error> {
        let timestamp = self.determine_timestamp(&source, when)?;

        let source = self.prep_relation_expr(
            source,
            ExprPrepStyle::OneShot {
                logical_time: timestamp,
            },
        )?;

        // If this optimizes to a constant expression, we can immediately return the result.
        let resp = if let RelationExpr::Constant { rows, typ: _ } = source.as_ref() {
            let mut results = Vec::new();
            for &(ref row, count) in rows {
                assert!(
                    count >= 0,
                    "Negative multiplicity in constant result: {}",
                    count
                );
                for _ in 0..count {
                    results.push(row.clone());
                }
            }
            finishing.finish(&mut results);
            send_immediate_rows(results)
        } else {
            // Peeks describe a source of data and a timestamp at which to view its contents.
            //
            // We need to determine both an appropriate timestamp from the description, and
            // also to ensure that there is a view in place to query, if the source of data
            // for the peek is not a base relation.

            // Choose a timestamp for all workers to use in the peek.
            // We minimize over all participating views, to ensure that the query will not
            // need to block on the arrival of further input data.
            let (rows_tx, rows_rx) = self.switchboard.mpsc_limited(self.num_timely_workers);

            // Extract any surrounding linear operators to determine if we can simply read
            // out the contents from an existing arrangement.
            let (mut map_filter_project, inner) =
                expr::MapFilterProject::extract_from_expression(source.as_ref());

            // We can use a fast path approach if our query corresponds to a read out of
            // an existing materialization. This is the case if the expression is now a
            // `RelationExpr::Get` and its target is something we have materialized.
            // Otherwise, we will need to build a new dataflow.
            let mut fast_path: Option<(_, Option<Row>)> = None;
            if let RelationExpr::Get {
                id: Id::Global(id),
                typ: _,
            } = inner
            {
                // Here we should check for an index whose keys are constrained to literal
                // values by predicate constraints in `map_filter_project`. If we find such
                // an index, we can use it with the literal to perform look-ups at workers,
                // and in principle avoid even contacting all but one worker (future work).
                if let Some(indexes) = self.catalog.indexes().get(id) {
                    // Determine for each index identifier, an optional row literal as key.
                    // We want to extract the "best" option, where we prefer indexes with
                    // literals and long keys, then indexes at all, then exit correctly.
                    fast_path = indexes
                        .iter()
                        .map(|(id, exprs)| {
                            let literal_row = map_filter_project.literal_constraints(exprs);
                            // Prefer non-trivial literal rows foremost, then long expressions,
                            // then we don't really care at that point.
                            (literal_row.is_some(), exprs.len(), literal_row, *id)
                        })
                        .max()
                        .map(|(_some, _len, literal, id)| (id, literal));
                }
            }

            // Unpack what we have learned with default values if we found nothing.
            let (fast_path, index_id, literal_row) = if let Some((id, row)) = fast_path {
                (true, id, row)
            } else {
                (false, self.allocate_transient_id()?, None)
            };

            if !fast_path {
                // Slow path. We need to perform some computation, so build
                // a new transient dataflow that will be dropped after the
                // peek completes.
                let typ = source.as_ref().typ();
                map_filter_project = expr::MapFilterProject::new(typ.arity());
                let key: Vec<_> = (0..typ.arity()).map(ScalarExpr::Column).collect();
                let view_id = self.allocate_transient_id()?;
                let mut dataflow = DataflowDesc::new(format!("temp-view-{}", view_id));
                dataflow.set_as_of(Antichain::from_elem(timestamp));
                self.dataflow_builder()
                    .import_view_into_dataflow(&view_id, &source, &mut dataflow);
                dataflow.add_index_to_build(index_id, view_id, typ.clone(), key.clone());
                dataflow.add_index_export(index_id, view_id, typ, key);
                self.ship_dataflow(dataflow).await;
            }

            broadcast(
                &mut self.broadcast_tx,
                SequencedCommand::Peek {
                    id: index_id,
                    key: literal_row,
                    conn_id,
                    tx: rows_tx,
                    timestamp,
                    finishing: finishing.clone(),
                    map_filter_project,
                },
            )
            .await;

            if !fast_path {
                self.drop_indexes(vec![index_id]).await;
            }

            let rows_rx = rows_rx
                .try_fold(PeekResponse::Rows(vec![]), |memo, resp| {
                    match (memo, resp) {
                        (PeekResponse::Rows(mut memo), PeekResponse::Rows(rows)) => {
                            memo.extend(rows);
                            future::ok(PeekResponse::Rows(memo))
                        }
                        (PeekResponse::Error(e), _) | (_, PeekResponse::Error(e)) => {
                            future::ok(PeekResponse::Error(e))
                        }
                        (PeekResponse::Canceled, _) | (_, PeekResponse::Canceled) => {
                            future::ok(PeekResponse::Canceled)
                        }
                    }
                })
                .map_ok(move |mut resp| {
                    if let PeekResponse::Rows(rows) = &mut resp {
                        finishing.finish(rows)
                    }
                    resp
                })
                .err_into();

            ExecuteResponse::SendingRows(Box::pin(rows_rx))
        };

        match copy_to {
            None => Ok(resp),
            Some(format) => Ok(ExecuteResponse::CopyTo {
                format,
                resp: Box::new(resp),
            }),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn sequence_tail(
        &mut self,
        session: &Session,
        source_id: GlobalId,
        with_snapshot: bool,
        ts: Option<Timestamp>,
        copy_to: Option<CopyFormat>,
        emit_progress: bool,
        object_columns: usize,
    ) -> Result<ExecuteResponse, anyhow::Error> {
        // Determine the frontier of updates to tail *from*.
        // Updates greater or equal to this frontier will be produced.
        let frontier = self.determine_frontier(ts, source_id)?;
        let sink_name = format!(
            "tail-source-{}",
            self.catalog
                .for_session(session)
                .humanize_id(source_id)
                .expect("Source id is known to exist in catalog")
        );
        let sink_id = self.catalog.allocate_id()?;
        self.active_tails.insert(session.conn_id(), sink_id);
        let (tx, rx) = self.switchboard.mpsc_limited(self.num_timely_workers);

        self.ship_dataflow(self.dataflow_builder().build_sink_dataflow(
            sink_name,
            sink_id,
            source_id,
            SinkConnector::Tail(TailSinkConnector {
                tx,
                frontier,
                strict: !with_snapshot,
                emit_progress,
                object_columns,
            }),
        ))
        .await;

        let resp = ExecuteResponse::Tailing { rx };

        match copy_to {
            None => Ok(resp),
            Some(format) => Ok(ExecuteResponse::CopyTo {
                format,
                resp: Box::new(resp),
            }),
        }
    }

    /// A policy for determining the timestamp for a peek.
    ///
    /// The result may be `None` in the case that the `when` policy cannot be satisfied,
    /// which is possible due to the restricted validity of traces (each has a `since`
    /// and `upper` frontier, and are only valid after `since` and sure to be available
    /// not after `upper`).
    fn determine_timestamp(
        &mut self,
        source: &RelationExpr,
        when: PeekWhen,
    ) -> Result<Timestamp, anyhow::Error> {
        // Each involved trace has a validity interval `[since, upper)`.
        // The contents of a trace are only guaranteed to be correct when
        // accumulated at a time greater or equal to `since`, and they
        // are only guaranteed to be currently present for times not
        // greater or equal to `upper`.
        //
        // The plan is to first determine a timestamp, based on the requested
        // timestamp policy, and then determine if it can be satisfied using
        // the compacted arrangements we have at hand. It remains unresolved
        // what to do if it cannot be satisfied (perhaps the query should use
        // a larger timestamp and block, perhaps the user should intervene).
        let uses_ids = &source.global_uses();
        let (index_ids, indexes_complete) = self.catalog.nearest_indexes(&uses_ids);

        // Determine the valid lower bound of times that can produce correct outputs.
        // This bound is determined by the arrangements contributing to the query,
        // and does not depend on the transitive sources.
        let since = self.indexes.least_valid_since(index_ids.iter().cloned());

        // First determine the candidate timestamp, which is either the explicitly requested
        // timestamp, or the latest timestamp known to be immediately available.
        let timestamp = match when {
            // Explicitly requested timestamps should be respected.
            PeekWhen::AtTimestamp(timestamp) => timestamp,

            // These two strategies vary in terms of which traces drive the
            // timestamp determination process: either the trace itself or the
            // original sources on which they depend.
            PeekWhen::Immediately => {
                if !indexes_complete {
                    bail!(
                        "Unable to automatically determine a timestamp for your query; \
                        this can happen if your query depends on non-materialized sources.\n\
                        For more details, see https://materialize.com/s/non-materialized-error"
                    );
                }
                let mut candidate = if uses_ids.iter().any(|id| self.catalog.uses_tables(*id)) {
                    // If the view depends on any tables, we enforce
                    // linearizability by choosing the latest input time.
                    self.get_read_ts()
                } else {
                    let upper = self.indexes.greatest_open_upper(index_ids.iter().copied());
                    // We peek at the largest element not in advance of `upper`, which
                    // involves a subtraction. If `upper` contains a zero timestamp there
                    // is no "prior" answer, and we do not want to peek at it as it risks
                    // hanging awaiting the response to data that may never arrive.
                    //
                    // The .get(0) here breaks the antichain abstraction by assuming this antichain
                    // has 0 or 1 elements in it. It happens to work because we use a timestamp
                    // type that meets that assumption, but would break if we used a more general
                    // timestamp.
                    if let Some(candidate) = upper.elements().get(0) {
                        if *candidate > 0 {
                            candidate.saturating_sub(1)
                        } else {
                            let unstarted = index_ids
                                .iter()
                                .filter(|id| {
                                    self.indexes
                                        .upper_of(id)
                                        .expect("id not found")
                                        .less_equal(&0)
                                })
                                .collect::<Vec<_>>();
                            bail!(
                                "At least one input has no complete timestamps yet: {:?}",
                                unstarted
                            );
                        }
                    } else {
                        // A complete trace can be read in its final form with this time.
                        //
                        // This should only happen for literals that have no sources
                        Timestamp::max_value()
                    }
                };
                // If the candidate is not beyond the valid `since` frontier,
                // force it to become so as best as we can. If `since` is empty
                // this will be a no-op, as there is no valid time, but that should
                // then be caught below.
                if !since.less_equal(&candidate) {
                    candidate.advance_by(since.borrow());
                }
                candidate
            }
        };

        // If the timestamp is greater or equal to some element in `since` we are
        // assured that the answer will be correct.
        if since.less_equal(&timestamp) {
            Ok(timestamp)
        } else {
            let invalid = index_ids
                .iter()
                .filter(|id| {
                    !self
                        .indexes
                        .since_of(id)
                        .expect("id not found")
                        .less_equal(&timestamp)
                })
                .map(|id| (id, self.indexes.since_of(id)))
                .collect::<Vec<_>>();
            bail!(
                "Timestamp ({}) is not valid for all inputs: {:?}",
                timestamp,
                invalid
            );
        }
    }

    /// Determine the frontier of updates to start *from*.
    /// Updates greater or equal to this frontier will be produced.
    fn determine_frontier(
        &mut self,
        as_of: Option<u64>,
        source_id: GlobalId,
    ) -> Result<Antichain<u64>, anyhow::Error> {
        let frontier = if let Some(ts) = as_of {
            // If a timestamp was explicitly requested, use that.
            Antichain::from_elem(self.determine_timestamp(
                &RelationExpr::Get {
                    id: Id::Global(source_id),
                    // TODO(justin): find a way to avoid synthesizing an arbitrary relation type.
                    typ: RelationType::empty(),
                },
                PeekWhen::AtTimestamp(ts),
            )?)
        }
        // TODO: The logic that follows is at variance from PEEK logic which consults the
        // "queryable" state of its inputs. We might want those to line up, but it is only
        // a "might".
        else if let Some(index_id) = self.catalog.default_index_for(source_id) {
            let upper = self
                .indexes
                .upper_of(&index_id)
                .expect("name missing at coordinator");

            if let Some(ts) = upper.get(0) {
                Antichain::from_elem(ts.saturating_sub(1))
            } else {
                Antichain::from_elem(Timestamp::max_value())
            }
        } else {
            // TODO: This should more carefully consider `since` frontiers of its input.
            // This will be forcibly corrected if any inputs are compacted.
            Antichain::from_elem(0)
        };
        Ok(frontier)
    }

    fn sequence_explain_plan(
        &mut self,
        session: &Session,
        raw_plan: sql::plan::RelationExpr,
        decorrelated_plan: expr::RelationExpr,
        row_set_finishing: Option<RowSetFinishing>,
        stage: ExplainStage,
        options: ExplainOptions,
    ) -> Result<ExecuteResponse, anyhow::Error> {
        let explanation_string = match stage {
            ExplainStage::RawPlan => {
                let catalog = self.catalog.for_session(session);
                let mut explanation = sql::plan::Explanation::new(&raw_plan, &catalog);
                if let Some(row_set_finishing) = row_set_finishing {
                    explanation.explain_row_set_finishing(row_set_finishing);
                }
                if options.typed {
                    explanation.explain_types(&BTreeMap::new());
                }
                explanation.to_string()
            }
            ExplainStage::DecorrelatedPlan => {
                let catalog = self.catalog.for_session(session);
                let mut explanation = expr::explain::Explanation::new(&decorrelated_plan, &catalog);
                if let Some(row_set_finishing) = row_set_finishing {
                    explanation.explain_row_set_finishing(row_set_finishing);
                }
                if options.typed {
                    explanation.explain_types();
                }
                explanation.to_string()
            }
            ExplainStage::OptimizedPlan => {
                let optimized_plan = self
                    .prep_relation_expr(decorrelated_plan, ExprPrepStyle::Explain)?
                    .into_inner();
                let catalog = self.catalog.for_session(session);
                let mut explanation = expr::explain::Explanation::new(&optimized_plan, &catalog);
                if let Some(row_set_finishing) = row_set_finishing {
                    explanation.explain_row_set_finishing(row_set_finishing);
                }
                if options.typed {
                    explanation.explain_types();
                }
                explanation.to_string()
            }
        };
        let rows = vec![Row::pack_slice(&[Datum::from(&*explanation_string)])];
        Ok(send_immediate_rows(rows))
    }

    async fn sequence_send_diffs(
        &mut self,
        id: GlobalId,
        updates: Vec<(Row, isize)>,
        affected_rows: usize,
        kind: MutationKind,
    ) -> Result<ExecuteResponse, anyhow::Error> {
        let timestamp = self.get_write_ts();
        let updates = updates
            .into_iter()
            .map(|(row, diff)| Update {
                row,
                diff,
                timestamp,
            })
            .collect();

        broadcast(
            &mut self.broadcast_tx,
            SequencedCommand::Insert { id, updates },
        )
        .await;

        Ok(match kind {
            MutationKind::Delete => ExecuteResponse::Deleted(affected_rows),
            MutationKind::Insert => ExecuteResponse::Inserted(affected_rows),
            MutationKind::Update => ExecuteResponse::Updated(affected_rows),
        })
    }

    async fn sequence_insert(
        &mut self,
        id: GlobalId,
        values: RelationExpr,
    ) -> Result<ExecuteResponse, anyhow::Error> {
        let prep_style = ExprPrepStyle::OneShot {
            logical_time: self.get_write_ts(),
        };
        match self.prep_relation_expr(values, prep_style)?.into_inner() {
            RelationExpr::Constant { rows, typ: _ } => {
                let desc = self.catalog.get_by_id(&id).desc()?;
                for (row, _) in &rows {
                    for (datum, (name, typ)) in row.unpack().iter().zip(desc.iter()) {
                        if datum == &Datum::Null && !typ.nullable {
                            bail!(
                                "null value in column \"{}\" violates not-null constraint",
                                name.unwrap_or(&ColumnName::from("unnamed column"))
                            )
                        }
                    }
                }

                let affected_rows = rows.len();
                self.sequence_send_diffs(id, rows, affected_rows, MutationKind::Insert)
                    .await
            }
            // If we couldn't optimize the INSERT statement to a constant, it
            // must depend on another relation. We're not yet sophisticated
            // enough to handle this.
            _ => bail!("INSERT statements cannot reference other relations"),
        }
    }

    async fn sequence_alter_item_rename(
        &mut self,
        id: Option<GlobalId>,
        to_name: String,
        object_type: ObjectType,
    ) -> Result<ExecuteResponse, anyhow::Error> {
        let id = match id {
            Some(id) => id,
            // None is generated by `IF EXISTS`
            None => return Ok(ExecuteResponse::AlteredObject(object_type)),
        };
        let op = catalog::Op::RenameItem { id, to_name };
        match self.catalog_transact(vec![op]).await {
            Ok(()) => Ok(ExecuteResponse::AlteredObject(object_type)),
            Err(err) => Err(err),
        }
    }

    fn sequence_alter_index_logical_compaction_window(
        &mut self,
        alter_index: Option<AlterIndexLogicalCompactionWindow>,
    ) -> Result<ExecuteResponse, anyhow::Error> {
        let (index, logical_compaction_window) = match alter_index {
            Some(AlterIndexLogicalCompactionWindow {
                index,
                logical_compaction_window,
            }) => (index, logical_compaction_window),
            // None is generated by `IF EXISTS` or if `logical_compaction_window`
            // was not found in ALTER INDEX ... RESET
            None => return Ok(ExecuteResponse::AlteredIndexLogicalCompaction),
        };

        let logical_compaction_window = match logical_compaction_window {
            LogicalCompactionWindow::Off => None,
            LogicalCompactionWindow::Default => self.logical_compaction_window_ms,
            LogicalCompactionWindow::Custom(window) => Some(duration_to_timestamp_millis(window)),
        };

        if let Some(index) = self.indexes.get_mut(&index) {
            index.set_compaction_window_ms(logical_compaction_window);
            Ok(ExecuteResponse::AlteredIndexLogicalCompaction)
        } else {
            // This can potentially happen if tries to delete the index and also
            // alter the index concurrently
            bail!("index {} not found", index.to_string())
        }
    }

    async fn catalog_transact(&mut self, ops: Vec<catalog::Op>) -> Result<(), anyhow::Error> {
        let events = self.catalog.transact(ops)?;
        self.process_catalog_events(events).await
    }

    async fn process_catalog_events(
        &mut self,
        events: Vec<catalog::Event>,
    ) -> Result<(), anyhow::Error> {
        let mut sources_to_drop = vec![];
        let mut sinks_to_drop = vec![];
        let mut indexes_to_drop = vec![];

        for event in &events {
            match event {
                catalog::Event::CreatedDatabase { id, oid, name } => {
                    self.report_database_update(*id, *oid, name, 1).await;
                }
                catalog::Event::CreatedSchema {
                    database_id,
                    schema_id,
                    schema_name,
                    oid,
                } => {
                    self.report_schema_update(*schema_id, *oid, *database_id, schema_name, 1)
                        .await;
                }
                catalog::Event::CreatedItem {
                    schema_id,
                    id,
                    oid,
                    name,
                    item,
                } => {
                    if let Ok(desc) = item.desc(&name) {
                        self.report_column_updates(desc, *id, 1).await?;
                    }
                    match item {
                        CatalogItem::Index(index) => {
                            self.report_index_update(*id, *oid, &index, &name.item, 1)
                                .await
                        }
                        CatalogItem::Table(_) => {
                            self.report_table_update(*id, *oid, *schema_id, &name.item, 1)
                                .await
                        }
                        CatalogItem::Source(_) => {
                            self.report_source_update(*id, *oid, *schema_id, &name.item, 1)
                                .await;
                        }
                        CatalogItem::View(_) => {
                            self.report_view_update(*id, *oid, *schema_id, &name.item, 1)
                                .await;
                        }
                        CatalogItem::Sink(sink) => {
                            if let catalog::Sink {
                                connector: SinkConnectorState::Ready(_),
                                ..
                            } = sink
                            {
                                self.report_sink_update(*id, *oid, *schema_id, &name.item, 1)
                                    .await;
                            }
                        }
                        CatalogItem::Type(ty) => {
                            self.report_type_update(*id, *oid, *schema_id, &name.item, ty, 1)
                                .await;
                        }
                    }
                }
                catalog::Event::UpdatedItem {
                    schema_id,
                    id,
                    oid,
                    from_name,
                    to_name,
                    item,
                } => {
                    // Remove old name and add new name to relevant mz system tables.
                    match item {
                        CatalogItem::Source(_) => {
                            self.report_source_update(*id, *oid, *schema_id, &from_name.item, -1)
                                .await;
                            self.report_source_update(*id, *oid, *schema_id, &to_name.item, 1)
                                .await;
                        }
                        CatalogItem::View(_) => {
                            self.report_view_update(*id, *oid, *schema_id, &from_name.item, -1)
                                .await;
                            self.report_view_update(*id, *oid, *schema_id, &to_name.item, 1)
                                .await;
                        }
                        CatalogItem::Sink(sink) => {
                            if let catalog::Sink {
                                connector: SinkConnectorState::Ready(_),
                                ..
                            } = sink
                            {
                                self.report_sink_update(*id, *oid, *schema_id, &from_name.item, -1)
                                    .await;
                                self.report_sink_update(*id, *oid, *schema_id, &to_name.item, 1)
                                    .await;
                            }
                        }
                        CatalogItem::Table(_) => {
                            self.report_table_update(*id, *oid, *schema_id, &from_name.item, -1)
                                .await;
                            self.report_table_update(*id, *oid, *schema_id, &to_name.item, 1)
                                .await;
                        }
                        CatalogItem::Index(index) => {
                            self.report_index_update(*id, *oid, &index, &from_name.item, -1)
                                .await;
                            self.report_index_update(*id, *oid, &index, &to_name.item, 1)
                                .await;
                        }
                        CatalogItem::Type(typ) => {
                            self.report_type_update(
                                *id,
                                *oid,
                                *schema_id,
                                &from_name.item,
                                &typ,
                                -1,
                            )
                            .await;
                            self.report_type_update(*id, *oid, *schema_id, &to_name.item, &typ, 1)
                                .await;
                        }
                    }
                }
                catalog::Event::DroppedDatabase { id, oid, name } => {
                    self.report_database_update(*id, *oid, name, -1).await;
                }
                catalog::Event::DroppedSchema {
                    database_id,
                    schema_id,
                    schema_name,
                    oid,
                } => {
                    self.report_schema_update(
                        *schema_id,
                        *oid,
                        Some(*database_id),
                        schema_name,
                        -1,
                    )
                    .await;
                }
                catalog::Event::DroppedIndex { entry, nullable } => match entry.item() {
                    CatalogItem::Index(index) => {
                        indexes_to_drop.push(entry.id());
                        self.report_index_update_inner(
                            entry.id(),
                            entry.oid(),
                            index,
                            &entry.name().item,
                            nullable.to_owned(),
                            -1,
                        )
                        .await
                    }
                    _ => unreachable!("DroppedIndex for non-index item"),
                },
                catalog::Event::DroppedItem { schema_id, entry } => {
                    match entry.item() {
                        CatalogItem::Table(_) => {
                            sources_to_drop.push(entry.id());
                            self.report_table_update(
                                entry.id(),
                                entry.oid(),
                                *schema_id,
                                &entry.name().item,
                                -1,
                            )
                            .await;
                        }
                        CatalogItem::Source(_) => {
                            sources_to_drop.push(entry.id());
                            self.report_source_update(
                                entry.id(),
                                entry.oid(),
                                *schema_id,
                                &entry.name().item,
                                -1,
                            )
                            .await;
                        }
                        CatalogItem::View(_) => {
                            self.report_view_update(
                                entry.id(),
                                entry.oid(),
                                *schema_id,
                                &entry.name().item,
                                -1,
                            )
                            .await;
                        }
                        CatalogItem::Sink(catalog::Sink {
                            connector: SinkConnectorState::Ready(connector),
                            ..
                        }) => {
                            sinks_to_drop.push(entry.id());
                            self.report_sink_update(
                                entry.id(),
                                entry.oid(),
                                *schema_id,
                                &entry.name().item,
                                -1,
                            )
                            .await;
                            match connector {
                                SinkConnector::Kafka(KafkaSinkConnector { topic, .. }) => {
                                    let row = Row::pack_slice(&[
                                        Datum::String(entry.id().to_string().as_str()),
                                        Datum::String(topic.as_str()),
                                    ]);
                                    self.update_catalog_view(
                                        MZ_KAFKA_SINKS.id,
                                        iter::once((row, -1)),
                                    )
                                    .await;
                                }
                                SinkConnector::AvroOcf(AvroOcfSinkConnector { path, .. }) => {
                                    let row = Row::pack_slice(&[
                                        Datum::String(entry.id().to_string().as_str()),
                                        Datum::Bytes(&path.clone().into_os_string().into_vec()),
                                    ]);
                                    self.update_catalog_view(
                                        MZ_AVRO_OCF_SINKS.id,
                                        iter::once((row, -1)),
                                    )
                                    .await;
                                }
                                _ => (),
                            }
                        }
                        CatalogItem::Sink(catalog::Sink {
                            connector: SinkConnectorState::Pending(_),
                            ..
                        }) => {
                            // If the sink connector state is pending, the sink
                            // dataflow was never created, so nothing to drop.
                        }
                        CatalogItem::Type(typ) => {
                            self.report_type_update(
                                entry.id(),
                                entry.oid(),
                                *schema_id,
                                &entry.name().item,
                                typ,
                                -1,
                            )
                            .await;
                        }
                        CatalogItem::Index(_) => {
                            unreachable!("dropped indexes should be handled by DroppedIndex");
                        }
                    }
                    if let Ok(desc) = entry.desc() {
                        self.report_column_updates(desc, entry.id(), -1).await?;
                    }
                }
                _ => (),
            }
        }

        if !sources_to_drop.is_empty() {
            broadcast(
                &mut self.broadcast_tx,
                SequencedCommand::DropSources(sources_to_drop),
            )
            .await;
        }
        if !sinks_to_drop.is_empty() {
            broadcast(
                &mut self.broadcast_tx,
                SequencedCommand::DropSinks(sinks_to_drop),
            )
            .await;
        }
        if !indexes_to_drop.is_empty() {
            self.drop_indexes(indexes_to_drop).await;
        }

        Ok(())
    }

    async fn drop_sinks(&mut self, dataflow_names: Vec<GlobalId>) {
        broadcast(
            &mut self.broadcast_tx,
            SequencedCommand::DropSinks(dataflow_names),
        )
        .await
    }

    async fn drop_indexes(&mut self, indexes: Vec<GlobalId>) {
        let mut trace_keys = Vec::new();
        for id in indexes {
            if self.indexes.remove(&id).is_some() {
                trace_keys.push(id);
            }
        }
        if !trace_keys.is_empty() {
            broadcast(
                &mut self.broadcast_tx,
                SequencedCommand::DropIndexes(trace_keys),
            )
            .await
        }
    }

    /// Prepares a relation expression for execution by preparing all contained
    /// scalar expressions (see `prep_scalar_expr`), then optimizing the
    /// relation expression.
    fn prep_relation_expr(
        &mut self,
        mut expr: RelationExpr,
        style: ExprPrepStyle,
    ) -> Result<OptimizedRelationExpr, anyhow::Error> {
        expr.try_visit_scalars_mut(&mut |s| Self::prep_scalar_expr(s, style))?;

        // TODO (wangandi): Is there anything that optimizes to a
        // constant expression that originally contains a global get? Is
        // there anything not containing a global get that cannot be
        // optimized to a constant expression?
        Ok(self.optimizer.optimize(expr, self.catalog.indexes())?)
    }

    /// Prepares a scalar expression for execution by replacing any placeholders
    /// with their correct values.
    ///
    /// Specifically, calls to the special function `MzLogicalTimestamp` are
    /// replaced according to `style`:
    ///
    ///   * if `OneShot`, calls are replaced according to the logical time
    ///     specified in the `OneShot` variant.
    ///   * if `Explain`, calls are replaced with a dummy time.
    ///   * if `Static`, calls trigger an error indicating that static queries
    ///     are not permitted to observe their own timestamps.
    fn prep_scalar_expr(expr: &mut ScalarExpr, style: ExprPrepStyle) -> Result<(), anyhow::Error> {
        // Replace calls to `MzLogicalTimestamp` as described above.
        let ts = match style {
            ExprPrepStyle::Explain | ExprPrepStyle::Static => 0, // dummy timestamp
            ExprPrepStyle::OneShot { logical_time } => logical_time,
        };
        let mut observes_ts = false;
        expr.visit_mut(&mut |e| {
            if let ScalarExpr::CallNullary(f @ NullaryFunc::MzLogicalTimestamp) = e {
                observes_ts = true;
                *e = ScalarExpr::literal_ok(Datum::from(i128::from(ts)), f.output_type());
            }
        });
        if observes_ts && matches!(style, ExprPrepStyle::Static) {
            bail!("mz_logical_timestamp cannot be used in static queries");
        }
        Ok(())
    }

    /// Finalizes a dataflow and then broadcasts it to all workers.
    ///
    /// Finalization includes optimization, but also validation of various
    /// invariants such as ensuring that the `as_of` frontier is in advance of
    /// the various `since` frontiers of participating data inputs.
    ///
    /// In particular, there are requirement on the `as_of` field for the dataflow
    /// and the `since` frontiers of created arrangements, as a function of the `since`
    /// frontiers of dataflow inputs (sources and imported arrangements).
    async fn ship_dataflow(&mut self, mut dataflow: DataflowDesc) {
        // The identity for `join` is the minimum element.
        let mut since = Antichain::from_elem(Timestamp::minimum());

        // TODO: Populate "valid from" information for each source.
        // For each source, ... do nothing because we don't track `since` for sources.
        // for (instance_id, _description) in dataflow.source_imports.iter() {
        //     // TODO: Extract `since` information about each source and apply here. E.g.
        //     since.join_assign(&self.source_info[instance_id].since);
        // }

        // For each imported arrangement, lower bound `since` by its own frontier.
        for (global_id, (_description, _typ)) in dataflow.index_imports.iter() {
            since.join_assign(
                self.indexes
                    .since_of(global_id)
                    .expect("global id missing at coordinator"),
            );
        }

        // For each produced arrangement, start tracking the arrangement with
        // a compaction frontier of at least `since`.
        for (global_id, _description, _typ) in dataflow.index_exports.iter() {
            let mut frontiers =
                Frontiers::new(self.num_timely_workers, self.logical_compaction_window_ms);
            frontiers.advance_since(&since);
            self.indexes.insert(*global_id, frontiers);
        }

        for (id, sink) in &dataflow.sink_exports {
            match &sink.connector {
                SinkConnector::Kafka(KafkaSinkConnector { topic, .. }) => {
                    let row = Row::pack_slice(&[
                        Datum::String(&id.to_string()),
                        Datum::String(topic.as_str()),
                    ]);
                    self.update_catalog_view(MZ_KAFKA_SINKS.id, iter::once((row, 1)))
                        .await;
                }
                SinkConnector::AvroOcf(AvroOcfSinkConnector { path, .. }) => {
                    let row = Row::pack_slice(&[
                        Datum::String(&id.to_string()),
                        Datum::Bytes(&path.clone().into_os_string().into_vec()),
                    ]);
                    self.update_catalog_view(MZ_AVRO_OCF_SINKS.id, iter::once((row, 1)))
                        .await;
                }
                _ => (),
            }
        }

        // TODO: Produce "valid from" information for each sink.
        // For each sink, ... do nothing because we don't yield `since` for sinks.
        // for (global_id, _description) in dataflow.sink_exports.iter() {
        //     // TODO: assign `since` to a "valid from" element of the sink. E.g.
        //     self.sink_info[global_id].valid_from(&since);
        // }

        // Ensure that the dataflow's `as_of` is at least `since`.
        if let Some(as_of) = &mut dataflow.as_of {
            // If we have requested a specific time that is invalid .. someone errored.
            use timely::order::PartialOrder;
            if !(<_ as PartialOrder>::less_equal(&since, as_of)) {
                // This can occur in SINK and TAIL at the moment. Their behaviors are fluid enough
                // that we just correct to avoid producing incorrect output updates, but we should
                // fix the root of the problem in a more principled manner.
                log::error!(
                    "Dataflow {} requested as_of ({:?}) not >= since ({:?}); correcting",
                    dataflow.debug_name,
                    as_of,
                    since
                );
                as_of.join_assign(&since);
            }
        } else {
            // Bind the since frontier to the dataflow description.
            dataflow.set_as_of(since);
        }

        // Optimize the dataflow across views, and any other ways that appeal.
        transform::optimize_dataflow(&mut dataflow);

        // Finalize the dataflow by broadcasting its construction to all workers.
        broadcast(
            &mut self.broadcast_tx,
            SequencedCommand::CreateDataflows(vec![dataflow]),
        )
        .await;
    }

    // Tell the cacher to start caching data for `id` if that source
    // has caching enabled and Materialize has caching enabled.
    // This function is a no-op if the cacher has already started caching
    // this source.
    async fn maybe_begin_caching(&mut self, id: GlobalId, source_connector: &SourceConnector) {
        if let SourceConnector::External { connector, .. } = source_connector {
            if connector.caching_enabled() {
                if let Some(cache_tx) = &mut self.cache_tx {
                    cache_tx
                        .send(CacheMessage::AddSource(
                            self.catalog.config().cluster_id,
                            id,
                        ))
                        .await
                        .expect("failed to send CREATE SOURCE notification to caching thread");
                } else {
                    log::error!(
                        "trying to create a cached source ({}) but caching is disabled.",
                        id
                    );
                }
            }
        }
    }

    fn allocate_transient_id(&mut self) -> Result<GlobalId, anyhow::Error> {
        let id = self.transient_id_counter;
        if id == u64::max_value() {
            bail!("id counter overflows i64");
        }
        self.transient_id_counter += 1;
        Ok(GlobalId::Transient(id))
    }
}

/// Begins coordinating user requests to the dataflow layer based on the
/// provided configuration. Returns the thread that hosts the coordinator and
/// the cluster ID.
///
/// To gracefully shut down the coordinator, send a `Message::Shutdown` to the
/// `cmd_rx` in the configuration, then join on the thread.
pub async fn serve<C>(
    Config {
        switchboard,
        cmd_rx,
        num_timely_workers,
        symbiosis_url,
        logging,
        data_directory,
        timestamp: timestamp_config,
        cache: cache_config,
        logical_compaction_window,
        experimental_mode,
        build_info,
    }: Config<'_, C>,
    // TODO(benesch): Don't pass runtime explicitly when
    // `Handle::current().block_in_place()` lands. See:
    // https://github.com/tokio-rs/tokio/pull/3097.
    runtime: Arc<Runtime>,
) -> Result<(JoinHandle<()>, Uuid), anyhow::Error>
where
    C: comm::Connection,
{
    let mut broadcast_tx = switchboard.broadcast_tx(dataflow::BroadcastToken);

    // First, configure the dataflow workers as directed by our configuration.
    // These operations must all be infallible.

    let (feedback_tx, feedback_rx) = switchboard.mpsc_limited(num_timely_workers);
    broadcast(
        &mut broadcast_tx,
        SequencedCommand::EnableFeedback(feedback_tx),
    )
    .await;

    if let Some(config) = &logging {
        broadcast(
            &mut broadcast_tx,
            SequencedCommand::EnableLogging(DataflowLoggingConfig {
                granularity_ns: config.granularity.as_nanos(),
                active_logs: BUILTINS
                    .logs()
                    .map(|src| (src.variant.clone(), src.index_id))
                    .collect(),
                log_logging: config.log_logging,
            }),
        )
        .await;
    }

    let cache_tx = if let Some(cache_config) = &cache_config {
        let (cache_tx, cache_rx) = switchboard.mpsc();
        broadcast(
            &mut broadcast_tx,
            SequencedCommand::EnableCaching(cache_tx.clone()),
        )
        .await;
        let cache_tx = cache_tx
            .connect()
            .await
            .expect("failed to connect cache tx");

        let mut cacher = Cacher::new(cache_rx, cache_config.clone());
        tokio::spawn(async move { cacher.run().await });

        Some(cache_tx)
    } else {
        None
    };

    // Then perform fallible operations, like opening the catalog. If these
    // fail, we are careful to tell the dataflow layer to shutdown.
    let coord = async {
        let symbiosis = if let Some(symbiosis_url) = symbiosis_url {
            Some(symbiosis::Postgres::open_and_erase(symbiosis_url).await?)
        } else {
            None
        };

        let path = data_directory.join("catalog");
        let (catalog, initial_catalog_events) = Catalog::open(&catalog::Config {
            path: &path,
            experimental_mode: Some(experimental_mode),
            enable_logging: logging.is_some(),
            cache_directory: cache_config.map(|c| c.path),
            build_info,
        })?;
        let cluster_id = catalog.config().cluster_id;

        let mut coord = Coordinator {
            broadcast_tx: switchboard.broadcast_tx(dataflow::BroadcastToken),
            switchboard: switchboard.clone(),
            num_timely_workers,
            optimizer: Default::default(),
            catalog,
            symbiosis,
            indexes: ArrangementFrontiers::default(),
            since_updates: Vec::new(),
            active_tails: HashMap::new(),
            logging_granularity: logging.and_then(|c| c.granularity.as_millis().try_into().ok()),
            timestamp_config,
            logical_compaction_window_ms: logical_compaction_window
                .map(duration_to_timestamp_millis),
            cache_tx,
            closed_up_to: 1,
            read_lower_bound: 1,
            last_op_was_read: false,
            need_advance: true,
            transient_id_counter: 1,
        };
        coord.bootstrap(initial_catalog_events).await?;
        Ok((coord, cluster_id))
    };
    let (coord, cluster_id) = match coord.await {
        Ok((coord, cluster_id)) => (coord, cluster_id),
        Err(e) => {
            broadcast(&mut broadcast_tx, SequencedCommand::Shutdown).await;
            return Err(e);
        }
    };

    // From this point on, this function must not fail! If you add a new
    // fallible operation, ensure it is in the async block above.

    // The future returned by `Coordinator::serve` does not implement `Send` as
    // it holds various non-thread-safe state across await points. This means we
    // can't use `tokio::spawn`, but instead have to spawn a dedicated thread to
    // run the future.
    Ok((
        thread::spawn(move || runtime.block_on(coord.serve(cmd_rx, feedback_rx))),
        cluster_id,
    ))
}

/// The styles in which an expression can be prepared.
#[derive(Clone, Copy, Debug)]
enum ExprPrepStyle {
    /// The expression is being prepared for output as part of an `EXPLAIN`
    /// query.
    Explain,
    /// The expression is being prepared for installation in a static context,
    /// like in a view.
    Static,
    /// The expression is being prepared to run once at the specified logical
    /// time.
    OneShot { logical_time: u64 },
}

async fn broadcast(tx: &mut comm::broadcast::Sender<SequencedCommand>, cmd: SequencedCommand) {
    // TODO(benesch): avoid flushing after every send.
    tx.send(cmd).await.unwrap();
}

/// Constructs an [`ExecuteResponse`] that that will send some rows to the
/// client immediately, as opposed to asking the dataflow layer to send along
/// the rows after some computation.
fn send_immediate_rows(rows: Vec<Row>) -> ExecuteResponse {
    let (tx, rx) = futures::channel::oneshot::channel();
    tx.send(PeekResponse::Rows(rows)).unwrap();
    ExecuteResponse::SendingRows(Box::pin(rx.err_into()))
}

fn auto_generate_primary_idx(
    index_name: String,
    on_name: FullName,
    on_id: GlobalId,
    on_desc: &RelationDesc,
) -> catalog::Index {
    let default_key = on_desc.typ().default_key();

    catalog::Index {
        create_sql: index_sql(index_name, on_name, &on_desc, &default_key),
        plan_cx: PlanContext::default(),
        on: on_id,
        keys: default_key.iter().map(|k| ScalarExpr::Column(*k)).collect(),
    }
}

// TODO(benesch): constructing the canonical CREATE INDEX statement should be
// the responsibility of the SQL package.
pub fn index_sql(
    index_name: String,
    view_name: FullName,
    view_desc: &RelationDesc,
    keys: &[usize],
) -> String {
    use sql::ast::{Expr, Ident, Value};

    CreateIndexStatement {
        name: Some(Ident::new(index_name)),
        on_name: sql::normalize::unresolve(view_name),
        key_parts: Some(
            keys.iter()
                .map(|i| match view_desc.get_unambiguous_name(*i) {
                    Some(n) => Expr::Identifier(vec![Ident::new(n.to_string())]),
                    _ => Expr::Value(Value::Number((i + 1).to_string())),
                })
                .collect(),
        ),
        if_not_exists: false,
    }
    .to_ast_string_stable()
}

// Convert a Duration to a Timestamp representing the number
// of milliseconds contained in that Duration
fn duration_to_timestamp_millis(d: Duration) -> Timestamp {
    let millis = d.as_millis();
    if millis > Timestamp::max_value() as u128 {
        Timestamp::max_value()
    } else if millis < Timestamp::min_value() as u128 {
        Timestamp::min_value()
    } else {
        millis as Timestamp
    }
}

/// Creates a description of the statement `stmt`.
///
/// This function is identical to sql::plan::describe except this is also
/// supports describing FETCH statements which need access to bound portals
/// through the session.
pub fn describe(
    catalog: &dyn sql::catalog::Catalog,
    stmt: Statement,
    param_types: &[Option<pgrepr::Type>],
    session: Option<&Session>,
) -> Result<StatementDesc, anyhow::Error> {
    match stmt {
        // FETCH's description depends on the current session, which describe_statement
        // doesn't (and shouldn't?) have access to, so intercept it here.
        Statement::Fetch(FetchStatement { ref name, .. }) => {
            match session
                .map(|session| session.get_portal(name.as_str()).map(|p| p.desc.clone()))
                .flatten()
            {
                Some(desc) => Ok(desc),
                // TODO(mjibson): return a correct error code here (34000) once our error
                // system supports it.
                None => bail!("cursor {} does not exist", name.to_ast_string_stable()),
            }
        }
        _ => sql::plan::describe(catalog, stmt, param_types),
    }
}

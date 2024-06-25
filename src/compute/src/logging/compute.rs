// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Logging dataflows for events generated by clusterd.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Write};
use std::rc::Rc;
use std::time::Duration;

use differential_dataflow::collection::AsCollection;
use differential_dataflow::trace::{BatchReader, Cursor};
use differential_dataflow::Collection;
use mz_ore::cast::CastFrom;
use mz_repr::{Datum, Diff, GlobalId, Timestamp};
use mz_timely_util::replay::MzReplay;
use timely::communication::Allocate;
use timely::container::CapacityContainerBuilder;
use timely::dataflow::channels::pact::Pipeline;
use timely::dataflow::channels::pushers::buffer::Session;
use timely::dataflow::channels::pushers::{Counter, Tee};
use timely::dataflow::operators::generic::builder_rc::OperatorBuilder;
use timely::dataflow::operators::{Filter, Operator};
use timely::dataflow::{Scope, Stream};
use timely::logging::WorkerIdentifier;
use timely::scheduling::Scheduler;
use timely::worker::Worker;
use timely::{Container, Data};
use tracing::error;
use uuid::Uuid;

use crate::extensions::arrange::MzArrange;
use crate::logging::{
    ComputeLog, EventQueue, LogCollection, LogVariant, PermutedRowPacker, SharedLoggingState,
};
use crate::typedefs::RowRowSpine;

/// Type alias for a logger of compute events.
pub type Logger = timely::logging_core::Logger<ComputeEvent, WorkerIdentifier>;

/// A logged compute event.
#[derive(Debug, Clone, PartialOrd, PartialEq)]
pub enum ComputeEvent {
    /// A dataflow export was created.
    Export {
        /// Identifier of the export.
        id: GlobalId,
        /// Timely worker index of the exporting dataflow.
        dataflow_index: usize,
    },
    /// A dataflow export was dropped.
    ExportDropped {
        /// Identifier of the export.
        id: GlobalId,
    },
    /// Peek command.
    Peek {
        /// The data for the peek itself.
        peek: Peek,
        /// The relevant _type_ of peek: index or persist.
        // Note that this is not stored on the Peek event for data-packing reasons only.
        peek_type: PeekType,
        /// True if the peek is being installed; false if it's being removed.
        installed: bool,
    },
    /// Available frontier information for dataflow exports.
    Frontier {
        id: GlobalId,
        time: Timestamp,
        diff: i8,
    },
    /// Available frontier information for dataflow imports.
    ImportFrontier {
        import_id: GlobalId,
        export_id: GlobalId,
        time: Timestamp,
        diff: i8,
    },
    /// Arrangement heap size update
    ArrangementHeapSize {
        /// Operator index
        operator: usize,
        /// Delta of the heap size in bytes of the arrangement.
        delta_size: isize,
    },
    /// Arrangement heap size update
    ArrangementHeapCapacity {
        /// Operator index
        operator: usize,
        /// Delta of the heap capacity in bytes of the arrangement.
        delta_capacity: isize,
    },
    /// Arrangement heap size update
    ArrangementHeapAllocations {
        /// Operator index
        operator: usize,
        /// Delta of distinct heap allocations backing the arrangement.
        delta_allocations: isize,
    },
    /// Arrangement size operator address
    ArrangementHeapSizeOperator {
        /// Operator index
        operator: usize,
        /// The address of the operator.
        address: Vec<usize>,
    },
    /// Arrangement size operator dropped
    ArrangementHeapSizeOperatorDrop {
        /// Operator index
        operator: usize,
    },
    /// All operators of a dataflow have shut down.
    DataflowShutdown {
        /// Timely worker index of the dataflow.
        dataflow_index: usize,
    },
    /// The number of errors in a dataflow export has changed.
    ErrorCount {
        /// Identifier of the export.
        export_id: GlobalId,
        /// The change in error count.
        diff: i64,
    },
}

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Debug)]
pub enum PeekType {
    Index,
    Persist,
}

impl PeekType {
    fn name(self) -> &'static str {
        match self {
            PeekType::Index => "index",
            PeekType::Persist => "persist",
        }
    }
}

/// A logged peek event.
#[derive(Debug, Clone, PartialOrd, PartialEq)]
pub struct Peek {
    /// The identifier of the view the peek targets.
    id: GlobalId,
    /// The logical timestamp requested.
    time: Timestamp,
    /// The ID of the peek.
    uuid: Uuid,
}

impl Peek {
    /// Create a new peek from its arguments.
    pub fn new(id: GlobalId, time: Timestamp, uuid: Uuid) -> Self {
        Self { id, time, uuid }
    }
}

/// Constructs the logging dataflow for compute logs.
///
/// Params
/// * `worker`: The Timely worker hosting the log analysis dataflow.
/// * `config`: Logging configuration.
/// * `event_queue`: The source to read compute log events from.
pub(super) fn construct<A: Allocate + 'static>(
    worker: &mut timely::worker::Worker<A>,
    config: &mz_compute_client::logging::LoggingConfig,
    event_queue: EventQueue<Vec<(Duration, WorkerIdentifier, ComputeEvent)>>,
    shared_state: Rc<RefCell<SharedLoggingState>>,
) -> BTreeMap<LogVariant, LogCollection> {
    let logging_interval_ms = std::cmp::max(1, config.interval.as_millis());
    let worker_id = worker.index();
    let worker2 = worker.clone();
    let dataflow_index = worker.next_dataflow_index();

    worker.dataflow_named("Dataflow: compute logging", move |scope| {
        let (mut logs, token) = Some(event_queue.link)
            .mz_replay::<_, CapacityContainerBuilder<_>, _>(
                scope,
                "compute logs",
                config.interval,
                event_queue.activator,
                |mut session, data| session.give_iterator(data.iter()),
            );

        // If logging is disabled, we still need to install the indexes, but we can leave them
        // empty. We do so by immediately filtering all logs events.
        if !config.enable_logging {
            logs = logs.filter(|_| false);
        }

        // Build a demux operator that splits the replayed event stream up into the separate
        // logging streams.
        let mut demux = OperatorBuilder::new("Compute Logging Demux".to_string(), scope.clone());
        let mut input = demux.new_input(&logs, Pipeline);
        let (mut export_out, export) = demux.new_output();
        let (mut frontier_out, frontier) = demux.new_output();
        let (mut import_frontier_out, import_frontier) = demux.new_output();
        let (mut peek_out, peek) = demux.new_output();
        let (mut peek_duration_out, peek_duration) = demux.new_output();
        let (mut shutdown_duration_out, shutdown_duration) = demux.new_output();
        let (mut arrangement_heap_size_out, arrangement_heap_size) = demux.new_output();
        let (mut arrangement_heap_capacity_out, arrangement_heap_capacity) = demux.new_output();
        let (mut arrangement_heap_allocations_out, arrangement_heap_allocations) =
            demux.new_output();
        let (mut error_count_out, error_count) = demux.new_output();

        let mut demux_state = DemuxState::new(worker2);
        let mut demux_buffer = Vec::new();
        demux.build(move |_capability| {
            move |_frontiers| {
                let mut export = export_out.activate();
                let mut frontier = frontier_out.activate();
                let mut import_frontier = import_frontier_out.activate();
                let mut peek = peek_out.activate();
                let mut peek_duration = peek_duration_out.activate();
                let mut shutdown_duration = shutdown_duration_out.activate();
                let mut arrangement_heap_size = arrangement_heap_size_out.activate();
                let mut arrangement_heap_capacity = arrangement_heap_capacity_out.activate();
                let mut arrangement_heap_allocations = arrangement_heap_allocations_out.activate();
                let mut error_count = error_count_out.activate();

                input.for_each(|cap, data| {
                    data.swap(&mut demux_buffer);

                    let mut output_sessions = DemuxOutput {
                        export: export.session(&cap),
                        frontier: frontier.session(&cap),
                        import_frontier: import_frontier.session(&cap),
                        peek: peek.session(&cap),
                        peek_duration: peek_duration.session(&cap),
                        shutdown_duration: shutdown_duration.session(&cap),
                        arrangement_heap_size: arrangement_heap_size.session(&cap),
                        arrangement_heap_capacity: arrangement_heap_capacity.session(&cap),
                        arrangement_heap_allocations: arrangement_heap_allocations.session(&cap),
                        error_count: error_count.session(&cap),
                    };

                    for (time, logger_id, event) in demux_buffer.drain(..) {
                        // We expect the logging infrastructure to not shuffle events between
                        // workers and this code relies on the assumption that each worker handles
                        // its own events.
                        assert_eq!(logger_id, worker_id);

                        DemuxHandler {
                            state: &mut demux_state,
                            shared_state: &mut shared_state.borrow_mut(),
                            output: &mut output_sessions,
                            logging_interval_ms,
                            time,
                        }
                        .handle(event);
                    }
                });
            }
        });

        // Encode the contents of each logging stream into its expected `Row` format.
        let mut packer = PermutedRowPacker::new(ComputeLog::DataflowCurrent);
        let dataflow_current = export.as_collection().map({
            let mut scratch = String::new();
            move |datum| {
                packer.pack_slice(&[
                    make_string_datum(datum.id, &mut scratch),
                    Datum::UInt64(u64::cast_from(worker_id)),
                    Datum::UInt64(u64::cast_from(datum.dataflow_id)),
                ])
            }
        });
        let mut packer = PermutedRowPacker::new(ComputeLog::FrontierCurrent);
        let frontier_current = frontier.as_collection().map({
            let mut scratch = String::new();
            move |datum| {
                packer.pack_slice(&[
                    make_string_datum(datum.export_id, &mut scratch),
                    Datum::UInt64(u64::cast_from(worker_id)),
                    Datum::MzTimestamp(datum.frontier),
                ])
            }
        });
        let mut packer = PermutedRowPacker::new(ComputeLog::ImportFrontierCurrent);
        let import_frontier_current = import_frontier.as_collection().map({
            let mut scratch1 = String::new();
            let mut scratch2 = String::new();
            move |datum| {
                packer.pack_slice(&[
                    make_string_datum(datum.export_id, &mut scratch1),
                    make_string_datum(datum.import_id, &mut scratch2),
                    Datum::UInt64(u64::cast_from(worker_id)),
                    Datum::MzTimestamp(datum.frontier),
                ])
            }
        });
        let mut packer = PermutedRowPacker::new(ComputeLog::PeekCurrent);
        let peek_current = peek.as_collection().map({
            let mut scratch = String::new();
            move |PeekDatum { peek, peek_type }| {
                packer.pack_slice(&[
                    Datum::Uuid(peek.uuid),
                    Datum::UInt64(u64::cast_from(worker_id)),
                    make_string_datum(peek.id, &mut scratch),
                    Datum::String(peek_type.name()),
                    Datum::MzTimestamp(peek.time),
                ])
            }
        });
        let mut packer = PermutedRowPacker::new(ComputeLog::PeekDuration);
        let peek_duration =
            peek_duration
                .as_collection()
                .map(move |PeekDurationDatum { peek_type, bucket }| {
                    packer.pack_slice(&[
                        Datum::UInt64(u64::cast_from(worker_id)),
                        Datum::String(peek_type.name()),
                        Datum::UInt64(bucket.try_into().expect("bucket too big")),
                    ])
                });
        let mut packer = PermutedRowPacker::new(ComputeLog::ShutdownDuration);
        let shutdown_duration = shutdown_duration.as_collection().map(move |bucket| {
            packer.pack_slice(&[
                Datum::UInt64(u64::cast_from(worker_id)),
                Datum::UInt64(bucket.try_into().expect("bucket too big")),
            ])
        });

        let arrangement_heap_datum_to_row =
            move |packer: &mut PermutedRowPacker, ArrangementHeapDatum { operator_id }| {
                packer.pack_slice(&[
                    Datum::UInt64(operator_id.try_into().expect("operator_id too big")),
                    Datum::UInt64(u64::cast_from(worker_id)),
                ])
            };

        let mut packer = PermutedRowPacker::new(ComputeLog::ArrangementHeapSize);
        let arrangement_heap_size = arrangement_heap_size
            .as_collection()
            .map(move |d| arrangement_heap_datum_to_row(&mut packer, d));

        let mut packer = PermutedRowPacker::new(ComputeLog::ArrangementHeapCapacity);
        let arrangement_heap_capacity = arrangement_heap_capacity
            .as_collection()
            .map(move |d| arrangement_heap_datum_to_row(&mut packer, d));

        let mut packer = PermutedRowPacker::new(ComputeLog::ArrangementHeapSize);
        let arrangement_heap_allocations = arrangement_heap_allocations
            .as_collection()
            .map(move |d| arrangement_heap_datum_to_row(&mut packer, d));

        let mut packer = PermutedRowPacker::new(ComputeLog::ErrorCount);
        let error_count = error_count.as_collection().map({
            let mut scratch = String::new();
            move |datum| {
                packer.pack_slice(&[
                    make_string_datum(datum.export_id, &mut scratch),
                    Datum::UInt64(u64::cast_from(worker_id)),
                    Datum::Int64(datum.count),
                ])
            }
        });

        use ComputeLog::*;
        let logs = [
            (DataflowCurrent, dataflow_current),
            (FrontierCurrent, frontier_current),
            (ImportFrontierCurrent, import_frontier_current),
            (PeekCurrent, peek_current),
            (PeekDuration, peek_duration),
            (ShutdownDuration, shutdown_duration),
            (ArrangementHeapSize, arrangement_heap_size),
            (ArrangementHeapCapacity, arrangement_heap_capacity),
            (ArrangementHeapAllocations, arrangement_heap_allocations),
            (ErrorCount, error_count),
        ];

        // Build the output arrangements.
        let mut result = BTreeMap::new();
        for (variant, collection) in logs {
            let variant = LogVariant::Compute(variant);
            if config.index_logs.contains_key(&variant) {
                let trace = collection
                    .mz_arrange::<RowRowSpine<_, _>>(&format!("Arrange {variant:?}"))
                    .trace;
                let collection = LogCollection {
                    trace,
                    token: Rc::clone(&token),
                    dataflow_index,
                };
                result.insert(variant, collection);
            }
        }

        result
    })
}

/// Format the given value and pack it into a `Datum::String`.
///
/// The `scratch` buffer is used to perform the string conversion without an allocation.
/// Callers should not assume anything about the contents of this buffer after this function
/// returns.
fn make_string_datum<V>(value: V, scratch: &mut String) -> Datum<'_>
where
    V: Display,
{
    scratch.clear();
    write!(scratch, "{}", value).expect("writing to a `String` can't fail");
    Datum::String(scratch)
}

/// State maintained by the demux operator.
struct DemuxState<A: Allocate> {
    /// The worker hosting this operator.
    worker: Worker<A>,
    /// State tracked per dataflow export.
    exports: BTreeMap<GlobalId, ExportState>,
    /// Maps live dataflows to counts of their exports.
    dataflow_export_counts: BTreeMap<usize, u32>,
    /// Maps dropped dataflows to their drop time.
    dataflow_drop_times: BTreeMap<usize, Duration>,
    /// Contains dataflows that have shut down but not yet been dropped.
    shutdown_dataflows: BTreeSet<usize>,
    /// Maps pending peeks to their installation time.
    peek_stash: BTreeMap<Uuid, Duration>,
    /// Arrangement size stash
    arrangement_size: BTreeMap<usize, ArrangementSizeState>,
}

impl<A: Allocate> DemuxState<A> {
    fn new(worker: Worker<A>) -> Self {
        Self {
            worker,
            exports: Default::default(),
            dataflow_export_counts: Default::default(),
            dataflow_drop_times: Default::default(),
            shutdown_dataflows: Default::default(),
            peek_stash: Default::default(),
            arrangement_size: Default::default(),
        }
    }
}

/// State tracked for each dataflow export.
struct ExportState {
    /// The ID of the dataflow maintaining this export.
    dataflow_id: usize,
    /// Number of errors in this export.
    ///
    /// This must be a signed integer, since per-worker error counts can be negative, only the
    /// cross-worker total has to sum up to a non-negative value.
    error_count: i64,
}

impl ExportState {
    fn new(dataflow_id: usize) -> Self {
        Self {
            dataflow_id,
            error_count: 0,
        }
    }
}

/// State for tracking arrangement sizes.
#[derive(Default)]
struct ArrangementSizeState {
    size: isize,
    capacity: isize,
    count: isize,
}

type Update<D> = (D, Timestamp, Diff);
type Pusher<D> = Counter<Timestamp, Vec<Update<D>>, Tee<Timestamp, Vec<Update<D>>>>;
type OutputSession<'a, D> =
    Session<'a, Timestamp, CapacityContainerBuilder<Vec<Update<D>>>, Pusher<D>>;

/// Bundled output sessions used by the demux operator.
struct DemuxOutput<'a> {
    export: OutputSession<'a, ExportDatum>,
    frontier: OutputSession<'a, FrontierDatum>,
    import_frontier: OutputSession<'a, ImportFrontierDatum>,
    peek: OutputSession<'a, PeekDatum>,
    peek_duration: OutputSession<'a, PeekDurationDatum>,
    shutdown_duration: OutputSession<'a, u128>,
    arrangement_heap_size: OutputSession<'a, ArrangementHeapDatum>,
    arrangement_heap_capacity: OutputSession<'a, ArrangementHeapDatum>,
    arrangement_heap_allocations: OutputSession<'a, ArrangementHeapDatum>,
    error_count: OutputSession<'a, ErrorCountDatum>,
}

#[derive(Clone)]
struct ExportDatum {
    id: GlobalId,
    dataflow_id: usize,
}

#[derive(Clone)]
struct FrontierDatum {
    export_id: GlobalId,
    frontier: Timestamp,
}

#[derive(Clone)]
struct ImportFrontierDatum {
    export_id: GlobalId,
    import_id: GlobalId,
    frontier: Timestamp,
}

#[derive(Clone)]
struct PeekDatum {
    peek: Peek,
    peek_type: PeekType,
}

#[derive(Clone)]
struct PeekDurationDatum {
    peek_type: PeekType,
    bucket: u128,
}

#[derive(Clone)]
struct ArrangementHeapDatum {
    operator_id: usize,
}

#[derive(Clone)]
struct ErrorCountDatum {
    export_id: GlobalId,
    // Normally we would use DD's diff field to encode counts, but in this case we can't: The total
    // per-worker error count might be negative and at the SQL level having negative multiplicities
    // is treated as an error.
    count: i64,
}

/// Event handler of the demux operator.
struct DemuxHandler<'a, 'b, A: Allocate + 'static> {
    /// State kept by the demux operator.
    state: &'a mut DemuxState<A>,
    /// State shared across log receivers.
    shared_state: &'a mut SharedLoggingState,
    /// Demux output sessions.
    output: &'a mut DemuxOutput<'b>,
    /// The logging interval specifying the time granularity for the updates.
    logging_interval_ms: u128,
    /// The current event time.
    time: Duration,
}

impl<A: Allocate> DemuxHandler<'_, '_, A> {
    /// Return the timestamp associated with the current event, based on the event time and the
    /// logging interval.
    fn ts(&self) -> Timestamp {
        let time_ms = self.time.as_millis();
        let interval = self.logging_interval_ms;
        let rounded = (time_ms / interval + 1) * interval;
        rounded.try_into().expect("must fit")
    }

    /// Handle the given compute event.
    fn handle(&mut self, event: ComputeEvent) {
        use ComputeEvent::*;

        match event {
            Export { id, dataflow_index } => self.handle_export(id, dataflow_index),
            ExportDropped { id } => self.handle_export_dropped(id),
            Peek {
                peek,
                peek_type,
                installed: true,
            } => self.handle_peek_install(peek, peek_type),
            Peek {
                peek,
                peek_type,
                installed: false,
            } => self.handle_peek_retire(peek, peek_type),
            Frontier { id, time, diff } => self.handle_frontier(id, time, diff),
            ImportFrontier {
                import_id,
                export_id,
                time,
                diff,
            } => self.handle_import_frontier(import_id, export_id, time, diff),
            ArrangementHeapSize {
                operator,
                delta_size: size,
            } => self.handle_arrangement_heap_size(operator, size),
            ArrangementHeapCapacity {
                operator,
                delta_capacity: capacity,
            } => self.handle_arrangement_heap_capacity(operator, capacity),
            ArrangementHeapAllocations {
                operator,
                delta_allocations: allocations,
            } => self.handle_arrangement_heap_allocations(operator, allocations),
            ArrangementHeapSizeOperator { operator, address } => {
                self.handle_arrangement_heap_size_operator(operator, address)
            }
            ArrangementHeapSizeOperatorDrop { operator } => {
                self.handle_arrangement_heap_size_operator_dropped(operator)
            }
            DataflowShutdown { dataflow_index } => self.handle_dataflow_shutdown(dataflow_index),
            ErrorCount { export_id, diff } => self.handle_error_count(export_id, diff),
        }
    }

    fn handle_export(&mut self, id: GlobalId, dataflow_id: usize) {
        let ts = self.ts();
        let datum = ExportDatum { id, dataflow_id };
        self.output.export.give((datum, ts, 1));

        self.state.exports.insert(id, ExportState::new(dataflow_id));
        *self
            .state
            .dataflow_export_counts
            .entry(dataflow_id)
            .or_default() += 1;
    }

    fn handle_export_dropped(&mut self, id: GlobalId) {
        let Some(export) = self.state.exports.remove(&id) else {
            error!(
                export = ?id,
                "missing exports entry at time of export drop"
            );
            return;
        };

        let ts = self.ts();
        let dataflow_id = export.dataflow_id;

        let datum = ExportDatum { id, dataflow_id };
        self.output.export.give((datum, ts, -1));

        match self.state.dataflow_export_counts.get_mut(&dataflow_id) {
            entry @ Some(0) | entry @ None => {
                error!(
                    export = ?id,
                    dataflow = ?dataflow_id,
                    "invalid dataflow_export_counts entry at time of export drop: {entry:?}",
                );
            }
            Some(1) => self.handle_dataflow_dropped(dataflow_id),
            Some(count) => *count -= 1,
        }

        // Remove error count logging for this export.
        if export.error_count != 0 {
            let datum = ErrorCountDatum {
                export_id: id,
                count: export.error_count,
            };
            self.output.error_count.give((datum, ts, -1));
        }
    }

    fn handle_dataflow_dropped(&mut self, id: usize) {
        self.state.dataflow_export_counts.remove(&id);

        if self.state.shutdown_dataflows.remove(&id) {
            // Dataflow has already shut down before it was dropped.
            self.output.shutdown_duration.give((0, self.ts(), 1));
        } else {
            // Dataflow has not yet shut down.
            let existing = self.state.dataflow_drop_times.insert(id, self.time);
            if existing.is_some() {
                error!(dataflow = ?id, "dataflow already dropped");
            }
        }
    }

    fn handle_dataflow_shutdown(&mut self, id: usize) {
        if let Some(start) = self.state.dataflow_drop_times.remove(&id) {
            // Dataflow has alredy been dropped.
            let elapsed_ns = self.time.saturating_sub(start).as_nanos();
            let elapsed_pow = elapsed_ns.next_power_of_two();
            self.output
                .shutdown_duration
                .give((elapsed_pow, self.ts(), 1));
        } else {
            // Dataflow has not yet been dropped.
            let was_new = self.state.shutdown_dataflows.insert(id);
            if !was_new {
                error!(dataflow = ?id, "dataflow already shutdown");
            }
        }
    }

    fn handle_error_count(&mut self, export_id: GlobalId, diff: i64) {
        let ts = self.ts();

        let Some(export) = self.state.exports.get_mut(&export_id) else {
            // The export might have already been dropped, in which case we are no longer
            // interested in its errors.
            return;
        };

        let old_count = export.error_count;
        let new_count = old_count + diff;

        if old_count != 0 {
            let datum = ErrorCountDatum {
                export_id,
                count: old_count,
            };
            self.output.error_count.give((datum, ts, -1));
        }
        if new_count != 0 {
            let datum = ErrorCountDatum {
                export_id,
                count: new_count,
            };
            self.output.error_count.give((datum, ts, 1));
        }

        export.error_count = new_count;
    }

    fn handle_peek_install(&mut self, peek: Peek, peek_type: PeekType) {
        let uuid = peek.uuid;
        let ts = self.ts();
        self.output
            .peek
            .give((PeekDatum { peek, peek_type }, ts, 1));

        let existing = self.state.peek_stash.insert(uuid, self.time);
        if existing.is_some() {
            error!(
                uuid = ?uuid,
                "peek already registered",
            );
        }
    }

    fn handle_peek_retire(&mut self, peek: Peek, peek_type: PeekType) {
        let uuid = peek.uuid;
        let ts = self.ts();
        self.output
            .peek
            .give((PeekDatum { peek, peek_type }, ts, -1));

        if let Some(start) = self.state.peek_stash.remove(&uuid) {
            let elapsed_ns = self.time.saturating_sub(start).as_nanos();
            let bucket = elapsed_ns.next_power_of_two();
            self.output
                .peek_duration
                .give((PeekDurationDatum { peek_type, bucket }, ts, 1));
        } else {
            error!(
                uuid = ?uuid,
                "peek not yet registered",
            );
        }
    }

    fn handle_frontier(&mut self, export_id: GlobalId, frontier: Timestamp, diff: i8) {
        let diff = i64::from(diff);
        let ts = self.ts();
        let datum = FrontierDatum {
            export_id,
            frontier,
        };
        self.output.frontier.give((datum, ts, diff));
    }

    fn handle_import_frontier(
        &mut self,
        import_id: GlobalId,
        export_id: GlobalId,
        frontier: Timestamp,
        diff: i8,
    ) {
        let ts = self.ts();
        let datum = ImportFrontierDatum {
            export_id,
            import_id,
            frontier,
        };
        self.output.import_frontier.give((datum, ts, diff.into()));
    }

    /// Update the allocation size for an arrangement.
    fn handle_arrangement_heap_size(&mut self, operator_id: usize, size: isize) {
        let ts = self.ts();
        let Some(state) = self.state.arrangement_size.get_mut(&operator_id) else {
            return;
        };

        let datum = ArrangementHeapDatum { operator_id };
        self.output
            .arrangement_heap_size
            .give((datum, ts, Diff::cast_from(size)));

        state.size += size;
    }

    /// Update the allocation capacity for an arrangement.
    fn handle_arrangement_heap_capacity(&mut self, operator_id: usize, capacity: isize) {
        let ts = self.ts();
        let Some(state) = self.state.arrangement_size.get_mut(&operator_id) else {
            return;
        };

        let datum = ArrangementHeapDatum { operator_id };
        self.output
            .arrangement_heap_capacity
            .give((datum, ts, Diff::cast_from(capacity)));

        state.capacity += capacity;
    }

    /// Update the allocation count for an arrangement.
    fn handle_arrangement_heap_allocations(&mut self, operator_id: usize, count: isize) {
        let ts = self.ts();
        let Some(state) = self.state.arrangement_size.get_mut(&operator_id) else {
            return;
        };

        let datum = ArrangementHeapDatum { operator_id };
        self.output
            .arrangement_heap_allocations
            .give((datum, ts, Diff::cast_from(count)));

        state.count += count;
    }

    /// Indicate that a new arrangement exists, start maintaining the heap size state.
    fn handle_arrangement_heap_size_operator(&mut self, operator_id: usize, address: Vec<usize>) {
        let activator = self.state.worker.activator_for(&address);
        self.state
            .arrangement_size
            .insert(operator_id, Default::default());
        self.shared_state
            .arrangement_size_activators
            .insert(operator_id, activator);
    }

    /// Indicate that an arrangement has been dropped and we can cleanup the heap size state.
    fn handle_arrangement_heap_size_operator_dropped(&mut self, operator_id: usize) {
        if let Some(state) = self.state.arrangement_size.remove(&operator_id) {
            let ts = self.ts();
            let datum = ArrangementHeapDatum { operator_id };
            self.output.arrangement_heap_size.give((
                datum.clone(),
                ts,
                -Diff::cast_from(state.size),
            ));
            self.output.arrangement_heap_capacity.give((
                datum.clone(),
                ts,
                -Diff::cast_from(state.capacity),
            ));
            self.output.arrangement_heap_allocations.give((
                datum,
                ts,
                -Diff::cast_from(state.count),
            ));
        }
        self.shared_state
            .arrangement_size_activators
            .remove(&operator_id);
    }
}

/// Logging state maintained for a compute collection.
///
/// This type is used to produce appropriate log events in response to changes of logged collection
/// state, e.g. frontiers, and to produce cleanup events when a collection is dropped.
pub struct CollectionLogging {
    id: GlobalId,
    logger: Logger,

    logged_frontier: Option<Timestamp>,
    logged_import_frontiers: BTreeMap<GlobalId, Timestamp>,
}

impl CollectionLogging {
    /// Create new logging state for the identified collection and emit initial logging events.
    pub fn new(
        id: GlobalId,
        logger: Logger,
        dataflow_index: usize,
        import_ids: impl Iterator<Item = GlobalId>,
    ) -> Self {
        logger.log(ComputeEvent::Export { id, dataflow_index });

        let mut self_ = Self {
            id,
            logger,
            logged_frontier: None,
            logged_import_frontiers: Default::default(),
        };

        // Initialize frontier logging.
        let initial_frontier = Some(Timestamp::MIN);
        self_.set_frontier(initial_frontier);
        import_ids.for_each(|id| self_.set_import_frontier(id, initial_frontier));

        self_
    }

    /// Set the collection frontier to the given new time and emit corresponding logging events.
    pub fn set_frontier(&mut self, new_time: Option<Timestamp>) {
        let old_time = self.logged_frontier;
        self.logged_frontier = new_time;

        if old_time != new_time {
            let id = self.id;
            let retraction = old_time.map(|time| ComputeEvent::Frontier { id, time, diff: -1 });
            let insertion = new_time.map(|time| ComputeEvent::Frontier { id, time, diff: 1 });
            let events = retraction.into_iter().chain(insertion);
            self.logger.log_many(events);
        }
    }

    /// Set the frontier of the given import to the given new time and emit corresponding logging
    /// events.
    pub fn set_import_frontier(&mut self, import_id: GlobalId, new_time: Option<Timestamp>) {
        let old_time = self.logged_import_frontiers.remove(&import_id);
        if let Some(time) = new_time {
            self.logged_import_frontiers.insert(import_id, time);
        }

        if old_time != new_time {
            let export_id = self.id;
            let retraction = old_time.map(|time| ComputeEvent::ImportFrontier {
                import_id,
                export_id,
                time,
                diff: -1,
            });
            let insertion = new_time.map(|time| ComputeEvent::ImportFrontier {
                import_id,
                export_id,
                time,
                diff: 1,
            });
            let events = retraction.into_iter().chain(insertion);
            self.logger.log_many(events);
        }
    }
}

impl Drop for CollectionLogging {
    fn drop(&mut self) {
        // Emit retraction events to clean up events previously logged.
        self.set_frontier(None);

        let import_ids: Vec<_> = self.logged_import_frontiers.keys().copied().collect();
        for id in import_ids {
            self.set_import_frontier(id, None);
        }

        self.logger.log(ComputeEvent::ExportDropped { id: self.id });
    }
}

/// Extension trait to attach `ComputeEvent::DataflowError` logging operators to collections and
/// batch streams.
pub(crate) trait LogDataflowErrors {
    fn log_dataflow_errors(self, logger: Logger, export_id: GlobalId) -> Self;
}

impl<G, D> LogDataflowErrors for Collection<G, D, Diff>
where
    G: Scope,
    D: Data,
{
    fn log_dataflow_errors(self, logger: Logger, export_id: GlobalId) -> Self {
        self.inner
            .unary(Pipeline, "LogDataflowErrorsCollection", |_cap, _info| {
                let mut buffer = Vec::new();
                move |input, output| {
                    input.for_each(|cap, data| {
                        data.swap(&mut buffer);

                        let diff = buffer.iter().map(|(_d, _t, r)| r).sum();
                        logger.log(ComputeEvent::ErrorCount { export_id, diff });

                        output.session(&cap).give_container(&mut buffer);
                    });
                }
            })
            .as_collection()
    }
}

impl<G, B> LogDataflowErrors for Stream<G, B>
where
    G: Scope,
    for<'a> B: BatchReader<DiffGat<'a> = &'a Diff> + Clone + 'static,
{
    fn log_dataflow_errors(self, logger: Logger, export_id: GlobalId) -> Self {
        self.unary(Pipeline, "LogDataflowErrorsStream", |_cap, _info| {
            let mut buffer = Vec::new();
            move |input, output| {
                input.for_each(|cap, data| {
                    data.swap(&mut buffer);

                    let diff = buffer.iter().map(sum_batch_diffs).sum();
                    logger.log(ComputeEvent::ErrorCount { export_id, diff });

                    output.session(&cap).give_container(&mut buffer);
                });
            }
        })
    }
}

/// Return the sum of all diffs within the given batch.
///
/// Note that this operation can be expensive: Its runtime is O(N) with N being the number of
/// unique (key, value, time) tuples. We only use it on error streams, which are expected to
/// contain only a small number of records, so this doesn't matter much. But avoid using it when
/// batches might become large.
fn sum_batch_diffs<B>(batch: &B) -> Diff
where
    for<'a> B: BatchReader<DiffGat<'a> = &'a Diff>,
{
    let mut sum = 0;
    let mut cursor = batch.cursor();

    while cursor.key_valid(batch) {
        while cursor.val_valid(batch) {
            cursor.map_times(batch, |_t, r| sum += r);
            cursor.step_val(batch);
        }
        cursor.step_key(batch);
    }

    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[mz_ore::test]
    fn test_compute_event_size() {
        // This could be a static assertion, but we don't use those yet in this crate.
        assert_eq!(48, std::mem::size_of::<ComputeEvent>())
    }
}

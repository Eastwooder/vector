//! Topology contains all topology based types.
//!
//! Topology is broken up into two main sections. The first
//! section contains all the main topology types include `Topology`
//! and the ability to start, stop and reload a config. The second
//! part contains config related items including config traits for
//! each type of component.

pub(super) use vector_core::fanout;
pub mod schema;

pub mod builder;
mod ready_arrays;
mod running;
mod task;

#[cfg(test)]
mod test;

use std::{
    collections::{HashMap, HashSet},
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
};

use futures::{Future, FutureExt};
pub(super) use running::RunningTopology;
use tokio::sync::{mpsc, watch};
use vector_buffers::topology::channel::{BufferReceiverStream, BufferSender};

use crate::{
    config::{ComponentKey, Config, ConfigDiff, OutputId},
    event::EventArray,
    topology::{
        builder::Pieces,
        task::{Task, TaskOutput},
    },
};

type TaskHandle = tokio::task::JoinHandle<Result<TaskOutput, ()>>;

type BuiltBuffer = (
    BufferSender<EventArray>,
    Arc<Mutex<Option<BufferReceiverStream<EventArray>>>>,
);

/// A tappable output consisting of an output ID and associated metadata
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct TapOutput {
    pub output_id: OutputId,
    pub component_kind: &'static str,
    pub component_type: String,
}

/// Resources used by the `tap` API to monitor component inputs and outputs,
/// updated alongside the topology
#[derive(Debug, Default, Clone)]
pub struct TapResource {
    // Outputs and their corresponding Fanout control
    pub outputs: HashMap<TapOutput, fanout::ControlChannel>,
    // Components (transforms, sinks) and their corresponding inputs
    pub inputs: HashMap<ComponentKey, Vec<OutputId>>,
    // Source component keys used to warn against invalid pattern matches
    pub source_keys: Vec<String>,
    // Sink component keys used to warn against invalid pattern amtches
    pub sink_keys: Vec<String>,
    // Components removed on a reload (used to drop TapSinks)
    pub removals: HashSet<ComponentKey>,
}

// Watcher types for topology changes.
type WatchTx = watch::Sender<TapResource>;
pub type WatchRx = watch::Receiver<TapResource>;

pub async fn start_validated(
    config: Config,
    diff: ConfigDiff,
    mut pieces: Pieces,
) -> Option<(RunningTopology, mpsc::UnboundedReceiver<()>)> {
    let (abort_tx, abort_rx) = mpsc::unbounded_channel();

    crate::metrics::Controller::get()
        .expect("Metrics must be initialized")
        .set_expiry(config.global.expire_metrics);

    let mut running_topology = RunningTopology::new(config, abort_tx);

    if !running_topology
        .run_healthchecks(&diff, &mut pieces, running_topology.config.healthchecks)
        .await
    {
        return None;
    }
    running_topology.connect_diff(&diff, &mut pieces).await;
    running_topology.spawn_diff(&diff, pieces);

    Some((running_topology, abort_rx))
}

pub async fn build_or_log_errors(
    config: &mut Config,
    diff: &ConfigDiff,
    buffers: HashMap<ComponentKey, BuiltBuffer>,
) -> Option<Pieces> {
    match builder::build_pieces(config, diff, buffers).await {
        Err(errors) => {
            for error in errors {
                error!(message = "Configuration error.", %error);
            }
            None
        }
        Ok(new_pieces) => Some(new_pieces),
    }
}

pub(super) fn take_healthchecks(
    diff: &ConfigDiff,
    pieces: &mut Pieces,
) -> Vec<(ComponentKey, Task)> {
    (&diff.sinks.to_change | &diff.sinks.to_add)
        .into_iter()
        .filter_map(|id| pieces.healthchecks.remove(&id).map(move |task| (id, task)))
        .collect()
}

async fn handle_errors(
    task: impl Future<Output = Result<TaskOutput, ()>>,
    abort_tx: mpsc::UnboundedSender<()>,
) -> Result<TaskOutput, ()> {
    AssertUnwindSafe(task)
        .catch_unwind()
        .await
        .map_err(|_| ())
        .and_then(|res| res)
        .map_err(|_| {
            error!("An error occurred that Vector couldn't handle.");
            let _ = abort_tx.send(());
        })
}

/// If the closure returns false, then the element is removed
fn retain<T>(vec: &mut Vec<T>, mut retain_filter: impl FnMut(&mut T) -> bool) {
    let mut i = 0;
    while let Some(data) = vec.get_mut(i) {
        if retain_filter(data) {
            i += 1;
        } else {
            let _ = vec.remove(i);
        }
    }
}

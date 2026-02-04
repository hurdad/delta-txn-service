use deltalake::action::{Action, Add, Remove};
use deltalake::kernel::DataChange;

use crate::grpc::pb;
use pb::action::Action as PbAction;

pub fn map_actions(pb_actions: Vec<pb::Action>) -> Result<Vec<Action>, String> {
    pb_actions
    .into_iter()
        .map(map_action)
        .collect()
}

fn map_action(action: pb::Action) -> Result<Action, String> {
    match action.action.ok_or("missing action")? {
        PbAction::Add(a) => Ok(Action::add(Add {
    path: a.path,
        size: a.size,
        modification_time: a.modification_time,
        partition_values: a.partition_values,
        data_change: map_data_change(a.data_change),
        stats: None,
        tags: a.tags,
})),

PbAction::Remove(r) => Ok(Action::remove(Remove {
    path: r.path,
        deletion_timestamp: r.deletion_timestamp,
        data_change: map_data_change(r.data_change),
        extended_file_metadata: None,
        partition_values: None,
        size: None,
        tags: None,
})),

_ => Err("unsupported action type".into()),
}
}

fn map_data_change(dc: i32) -> bool {
    matches!(
        pb::DataChange::from_i32(dc),
            Some(pb::DataChange::DataChangeTrue)
    )
}

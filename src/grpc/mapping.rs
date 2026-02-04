use deltalake::kernel::{Action, Add, Remove};

use crate::grpc::server::pb;
use pb::action::Action as PbAction;

pub fn map_actions(pb_actions: Vec<pb::Action>) -> Result<Vec<Action>, String> {
    pb_actions.into_iter().map(map_action).collect()
}

fn map_action(action: pb::Action) -> Result<Action, String> {
    match action.action.ok_or("missing action")? {
        PbAction::Add(a) => Ok(Action::Add(Add {
            path: a.path,
            size: a.size,
            modification_time: a.modification_time,
            partition_values: map_string_map(a.partition_values),
            data_change: map_data_change(a.data_change),
            stats: None,
            tags: map_optional_string_map(a.tags),
            deletion_vector: None,
            base_row_id: None,
            default_row_commit_version: None,
            clustering_provider: None,
        })),

        PbAction::Remove(r) => Ok(Action::Remove(Remove {
            path: r.path,
            deletion_timestamp: r.deletion_timestamp,
            data_change: map_data_change(r.data_change),
            extended_file_metadata: None,
            partition_values: None,
            size: None,
            tags: None,
            deletion_vector: None,
            base_row_id: None,
            default_row_commit_version: None,
        })),

        _ => Err("unsupported action type".into()),
    }
}

fn map_data_change(dc: i32) -> bool {
    matches!(pb::DataChange::try_from(dc), Ok(pb::DataChange::True))
}

fn map_string_map(
    input: std::collections::HashMap<String, String>,
) -> std::collections::HashMap<String, Option<String>> {
    input.into_iter().map(|(k, v)| (k, Some(v))).collect()
}

fn map_optional_string_map(
    input: std::collections::HashMap<String, String>,
) -> Option<std::collections::HashMap<String, Option<String>>> {
    if input.is_empty() {
        None
    } else {
        Some(map_string_map(input))
    }
}

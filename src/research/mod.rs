//! Research policy over the durable tool journal. Evidence and interpretation
//! remain distinct; filesystem operations belong to builder-tools.
mod evidence;
mod execution;
mod retrieval;

use builder_tools::Workspace;
pub use execution::{execute, execute_with_settings};
pub use retrieval::{
    packet, packet_with_settings, packet_with_settings_for_phase, status, status_with_settings,
};

fn scope(workspace: &Workspace) -> String {
    workspace.root().to_string_lossy().into_owned()
}

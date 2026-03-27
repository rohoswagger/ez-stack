use anyhow::Result;
use std::collections::HashMap;

use crate::git;
use crate::stack::StackState;
use crate::ui;

pub fn run() -> Result<()> {
    let state = StackState::load()?;
    let current = git::current_branch()?;

    // Build branch→worktree path map.
    let worktree_map: HashMap<String, String> = git::worktree_list()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|wt| wt.branch.map(|b| (b, wt.path)))
        .collect();

    // Show trunk first.
    let trunk_marker = if current == state.trunk { "* " } else { "  " };
    let trunk_wt = worktree_map
        .get(state.trunk.as_str())
        .map(|p| format!(" {p}"))
        .unwrap_or_default();
    println!("{trunk_marker}{} (trunk){trunk_wt}", state.trunk);

    // Show all managed branches in topo order.
    let order = state.topo_order();
    for branch in &order {
        let meta = state.get_branch(branch)?;
        let marker = if *branch == current { "* " } else { "  " };
        let pr = meta.pr_number.map(|n| format!(" #{n}")).unwrap_or_default();
        let wt = worktree_map
            .get(branch.as_str())
            .map(|p| format!(" {p}"))
            .unwrap_or_default();
        println!("{marker}{branch}{pr}{wt}");
    }

    // If current branch is not trunk and not managed, show it with a warning.
    if current != state.trunk && !state.is_managed(&current) {
        let wt = worktree_map
            .get(current.as_str())
            .map(|p| format!(" {p}"))
            .unwrap_or_default();
        println!("* {current} (not tracked by ez){wt}");
        ui::hint(&format!(
            "`{current}` was created outside ez — use `ez create` to track branches"
        ));
    }

    Ok(())
}

use anyhow::Result;

/// `ez worktree create` is now an alias for `ez create` (worktree is the default).
pub fn create(name: &str, from: Option<&str>) -> Result<()> {
    crate::cmd::create::run(name, None, false, false, from, false, &[], None, None)
}

// `ez worktree delete` → routed to cmd::delete::run() in main.rs
// `ez worktree list`   → routed to cmd::list::run() in main.rs

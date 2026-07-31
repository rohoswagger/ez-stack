# Contributing to ez

Thanks for your interest in contributing to `ez`! This guide covers everything you need to get started.

## Development setup

```bash
# Clone the repo
git clone https://github.com/rohoswagger/ez-stack.git
cd ez-stack

# Build
cargo build

# Run tests
cargo test

# Run the CLI locally
cargo run -- <command>
```

You'll also need `git` and `gh` (GitHub CLI) installed and authenticated for integration tests.

## Code style

We enforce consistent style with standard Rust tooling:

```bash
# Format code
cargo fmt

# Lint
cargo clippy -- -D warnings
```

Both checks run in CI. Please run them locally before submitting a PR.

### General guidelines

- Keep functions short and focused.
- Prefer returning `Result` over panicking.
- Write doc comments for all public types and functions.
- Use `thiserror` for error types. Avoid `anyhow` in library code.

## How to submit changes

1. Fork the repository and create a branch from `main`.
2. Make your changes, add tests where appropriate.
3. Run `cargo fmt`, `cargo clippy`, and `cargo test`.
4. Open a pull request against `main` with a clear description of what you changed and why.

For larger changes, please open an issue first to discuss the approach.

## Project structure

```
src/
├── main.rs          # Entry point and command dispatch
├── cli.rs           # CLI argument parsing (clap derive)
├── cmd/             # One module per command (create.rs, sync.rs, push.rs, etc.)
├── stack.rs         # Stack state model and persistence (.git/ez/stack.json)
├── git.rs           # Git shell-out operations
├── github.rs        # Wrapper functions for shelling out to gh
├── ui.rs            # Terminal colors, spinners, tree rendering
└── error.rs         # thiserror error types
```

### Key files

| File | Purpose |
|------|---------|
| `src/main.rs` | Entry point and command dispatch. |
| `src/cli.rs` | CLI argument parsing using clap derive macros. |
| `src/stack.rs` | Defines the `Stack` and `Branch` structs. Handles serialization and deserialization of `.git/ez/stack.json`. |
| `src/git.rs` | Thin wrappers around `git` commands (`rebase`, `checkout`, `branch`, etc.) using `std::process::Command`. |
| `src/github.rs` | Thin wrappers around `gh` commands (`pr create`, `pr edit`, `pr view`, etc.). |
| `src/ui.rs` | Terminal colors, spinners, and tree rendering for stack visualization. |
| `src/error.rs` | Error types defined with `thiserror`. |
| `src/cmd/` | Each command is a module that takes parsed CLI args and orchestrates calls to `stack`, `git`, and `github`. |

## Architecture decisions

### Why shell out to git and gh?

We deliberately call the `git` and `gh` CLIs as subprocesses rather than using `libgit2` or the GitHub API directly. Reasons:

- **Transparency.** Users can see exactly what commands ran. If something goes wrong, they can reproduce or fix it with the same tools.
- **Correctness.** `git rebase` has complex behavior around conflict resolution, hooks, and config. Reimplementing it is a source of bugs. Shelling out gets us the real thing.
- **Simplicity.** No OAuth token management, no REST/GraphQL client, no C bindings. `gh` handles auth and API versioning for us.

The tradeoff is a dependency on both CLIs being installed and some overhead from process spawning, which is negligible for a developer tool.

### Why .git/ez/stack.json?

- Stored inside `.git/` so it's never accidentally committed.
- Single file so reads and writes are atomic (via write-to-temp + rename).
- JSON so it's human-readable and easy to debug. If `ez` ever gets into a bad state, you can edit the file by hand.
- Versioned with a `"version"` field so we can migrate the format in the future without breaking existing repos.

### Why rebase --onto?

When a parent branch is updated (e.g., after a sync), child branches need to move. `git rebase --onto` is the precise tool for this:

```
git rebase --onto <new-parent> <old-parent> <child>
```

This replays only the commits unique to `<child>` onto `<new-parent>`, which is exactly the semantics we want when restacking.

## Running tests

```bash
# Unit tests
cargo test

# Run a specific test
cargo test test_stack_serialization

# Integration tests (requires git and gh)
cargo test --test integration

# Stable-toolchain coverage (requires cargo-llvm-cov)
cargo llvm-cov --all-features --workspace --no-fail-fast --summary-only
```

Integration tests create temporary git repos and exercise full command flows. They do not touch GitHub — any `gh` calls are stubbed.

CI enforces monotonically increasing line, function, and region coverage floors.
The current ratchet is 79% lines, 83% functions, and 79% regions, with 100% as
the target. Do not lower a floor to make a change pass; add behavior-focused
tests or simplify unreachable production code, then raise the floor whenever
the measured whole-workspace result crosses the next integer.

## Questions?

Open an issue or start a discussion on the repository. We're happy to help.

# Swarm Rust SDK Template

This directory contains the `swarm-sdk` template for writing Rust-based bots for Swarm.

## Project Structure

- `src/lib.rs`: The main entry point for the SDK. It exports the core `tick` function and types.
- `src/bot.rs`: Where you implement your bot logic.
- `src/commands.rs`: Auto-generated command types, directions, and body parts (do not edit).
- `src/types_template.rs`: Stable SDK types including `Snapshot`, `ObjectKind`, and `TickResult`.

## Implementation Details

### Tick Function

Edit `src/bot.rs` to implement the template-local bot function. `src/lib.rs` already exports the public `tick` entry point and forwards to this function:

```rust
use crate::{Snapshot, TickResult};

pub fn tick(snapshot: Snapshot) -> TickResult {
    let _ = snapshot;
    TickResult { commands: Vec::new() }
}
```

- **`Snapshot`**: Contains the current state of the world, including your drones, structures, and visible objects.
- **`TickResult`**: Wraps a list of `Command` objects to be executed by the engine.

### Commands

Drones are controlled by returning `Command` objects. The `CommandAction` enum contains all valid actions:
- `Move { object_id, direction }`
- `Harvest { object_id, target_id, resource }`
- `Transfer { object_id, target_id, resource, amount }`
- `Spawn { object_id, spawn_id, body_parts }`
- And more (see `src/commands.rs`).

Note: `TickResult` is a convenience wrapper inside this Rust template. The engine wire contract remains the serialized command-intent array contained in `TickResult.commands`.

## Development

Use the standard Rust toolchain to develop and test your bot.

### Commands

```bash
# Verify the code compiles
cargo check

# Run the local test suite
cargo test
```

Start from the implemented example in `src/bot.rs`, then use the generated types from `src/commands.rs` to append commands with monotonically increasing `sequence` values.

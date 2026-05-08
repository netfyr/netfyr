# Spec: Server Architecture for Concurrent Clients

## Problem

The Varlink server in `netfyr-daemon` processes connections sequentially.
The `serve_varlink()` event loop calls `handle_connection().await` inline
in `tokio::select!`, which blocks all other event sources (factory events,
netlink monitor, new connections) until the client disconnects.

This prevents:
- Multiple clients connecting simultaneously
- Factory events (DHCP lease changes) being processed while a client is
  connected
- Future streaming features (Monitor method for GUI clients)

## Goals

1. Accept and handle multiple Varlink connections concurrently.
2. Process factory events and netlink monitor events without waiting for
   active connections to close.
3. Add a broadcast channel that publishes daemon events, so that future
   streaming (Monitor) connections can subscribe to state changes.
4. Preserve existing behavior for all current Varlink methods.

## Design

### Shared state

Introduce a `DaemonState` struct that bundles the mutable server state:

```rust
struct DaemonState {
    policy_store: PolicyStore,
    factory_manager: FactoryManager,
    managed_entities: HashSet<String>,
}
```

Wrap it in `Arc<tokio::sync::Mutex<DaemonState>>`. The `Reconciler` is
already internally thread-safe (`Mutex<Journal>`, `Arc<AtomicBool>`) and
only requires `&self`, so it is wrapped in `Arc<Reconciler>` (no Mutex).

### Factory event receiver extraction

`FactoryManager` currently owns the `mpsc::Receiver<FactoryEvent>`.
Since we cannot hold the `DaemonState` mutex across an async
`recv().await` in the select loop, the receiver must live outside the
mutex.

Add a public method to `FactoryManager`:

```rust
pub fn take_event_receiver(&mut self) -> mpsc::Receiver<FactoryEvent>
```

This is called once during server initialization, before wrapping
`FactoryManager` in the mutex. The receiver moves into the select loop.
The `FactoryManager` retains the `mpsc::Sender` (cloned into each
factory task) and continues to function normally.

### Connection spawning

Change the connection-accept branch from blocking inline handling to
spawning a task:

```rust
accept_result = listener.accept() => {
    let state = Arc::clone(&state);
    let reconciler = Arc::clone(&reconciler);
    let event_tx = event_tx.clone();
    tokio::spawn(async move {
        handle_connection(&mut stream, &state, &reconciler,
                          start_time, &event_tx).await;
    });
}
```

Each `handle_*` function acquires the `DaemonState` mutex for the
duration of its work, then drops the guard before writing the response
to the stream.

### Broadcast channel

Add a `tokio::sync::broadcast` channel for daemon events:

```rust
#[derive(Clone, Debug)]
enum DaemonEvent {
    PolicyChanged,
    DhcpEvent { interface: String, kind: String },
    ExternalChange { interfaces: Vec<String> },
}
```

Created in `serve_varlink()` with capacity 64:

```rust
let (event_tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(64);
```

The event loop publishes to `event_tx` after key state changes:
- After `SubmitPolicies` completes: `PolicyChanged`
- After processing a factory event: `DhcpEvent { interface, kind }`
- After recording an external change: `ExternalChange { interfaces }`

`.send().ok()` is used because send fails only when no receivers are
subscribed, which is expected until Monitor is implemented.

The `event_tx` is passed to `handle_connection` so that write handlers
can also publish `PolicyChanged` after modifying policies.

### Managed entities refresh

Currently, `managed_entities` is refreshed synchronously after each
connection completes. With spawned connections, this is no longer
possible from the main loop.

Instead, `managed_entities` lives inside `DaemonState`. Handlers that
change policies (SubmitPolicies, and the future AddPolicy/RemovePolicy)
recompute `managed_entities` while holding the lock. The netlink monitor
branch reads it via `state.lock().await`.

### Handler signature changes

All `handle_*` functions change from taking individual `&mut` references
to taking shared references:

```rust
// Before
async fn handle_get_status(
    stream: &mut UnixStream,
    policy_store: &PolicyStore,
    factory_manager: &FactoryManager,
    start_time: Instant,
) -> Result<()>

// After
async fn handle_get_status(
    stream: &mut UnixStream,
    state: &tokio::sync::Mutex<DaemonState>,
    start_time: Instant,
) -> Result<()>
```

Similarly, `handle_connection` changes to:

```rust
async fn handle_connection(
    stream: &mut UnixStream,
    state: &tokio::sync::Mutex<DaemonState>,
    reconciler: &Reconciler,
    start_time: Instant,
    event_tx: &broadcast::Sender<DaemonEvent>,
)
```

### Lock discipline

Each handler acquires the mutex, does its work, and drops the guard:

```rust
async fn handle_get_status(
    stream: &mut UnixStream,
    state: &tokio::sync::Mutex<DaemonState>,
    start_time: Instant,
) -> Result<()> {
    let status = {
        let guard = state.lock().await;
        // ... build status from guard.policy_store, guard.factory_manager
        status
    }; // guard dropped here
    write_success(stream, serde_json::json!({ "status": status })).await
}
```

The lock is never held across stream I/O. This ensures that:
- Read handlers can run concurrently with each other (they acquire and
  release the lock quickly)
- Write handlers get exclusive access to the policy store and factory
  manager
- The main event loop can process factory/netlink events between handler
  lock acquisitions

### Event loop changes

The main `tokio::select!` loop changes as follows:

```rust
let mut factory_event_rx = factory_manager.take_event_receiver();
let state = Arc::new(tokio::sync::Mutex::new(DaemonState {
    policy_store,
    factory_manager,
    managed_entities,
}));
let reconciler = Arc::new(reconciler);
let (event_tx, _) = broadcast::channel::<DaemonEvent>(64);

loop {
    tokio::select! {
        // Branch 1: incoming connection — spawn task
        accept_result = listener.accept() => {
            let state = Arc::clone(&state);
            let reconciler = Arc::clone(&reconciler);
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                handle_connection(&mut stream, &state, &reconciler,
                                  start_time, &event_tx).await;
            });
        }

        // Branch 2: factory event — lock state, reconcile
        Some(event) = factory_event_rx.recv() => {
            let mut guard = state.lock().await;
            // ... handle event using guard.policy_store,
            //     guard.factory_manager, reconciler
            // ... refresh guard.managed_entities
            drop(guard);
            event_tx.send(DaemonEvent::DhcpEvent { ... }).ok();
        }

        // Branch 3-4: signals (unchanged)

        // Branch 5: netlink monitor — lock state for managed_entities
        result = netlink_monitor.next_change() => {
            let guard = state.lock().await;
            let managed = guard.managed_entities.clone();
            drop(guard);
            // ... filter changes against managed, record external change
            event_tx.send(DaemonEvent::ExternalChange { ... }).ok();
        }
    }
}
```

## Files to modify

| File | Changes |
|------|---------|
| `crates/netfyr-daemon/src/server.rs` | DaemonState struct, Arc/Mutex wrapping, spawn connections, broadcast channel, handler signature updates, event loop restructuring |
| `crates/netfyr-daemon/src/factory_manager.rs` | Add `take_event_receiver()` method |

## Testing

- Existing unit tests in `server.rs` must be updated to pass
  `&Mutex<DaemonState>` and `&Reconciler` instead of individual refs.
  The `make_stream_pair()` pattern remains the same.
- Integration test: ten clients connect simultaneously, all receive
  correct responses.
- Integration test: factory event is processed while a client connection
  is active.
- `cargo test` passes with no regressions.

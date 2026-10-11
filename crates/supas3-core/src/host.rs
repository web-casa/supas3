//! Stand-ins for the Tauri types the upstream backend was written against.
//!
//! Keeping the names (`State`, `AppHandle`, `async_runtime`) lets the imported
//! command functions stay textually close to upstream, which keeps later
//! upstream fixes easy to port. Nothing here depends on a UI toolkit.

use serde::Serialize;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, RwLock};

/// A borrowed handle to one piece of shared backend state.
///
/// The reference is kept in a named private field so that `state.0` reaches
/// through `Deref` to the wrapped value, as upstream code expects.
pub struct State<'a, T: ?Sized> {
    value: &'a T,
}

impl<'a, T: ?Sized> State<'a, T> {
    pub fn new(value: &'a T) -> Self {
        Self { value }
    }

    pub fn inner(&self) -> &'a T {
        self.value
    }
}

// Clone but deliberately not Copy: upstream calls `.clone()` on its state
// handles, and a Copy type would make every one of those a lint warning.
impl<T: ?Sized> Clone for State<'_, T> {
    fn clone(&self) -> Self {
        Self { value: self.value }
    }
}

impl<T: ?Sized> Deref for State<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.value
    }
}

/// An owned copy of managed state, returned by [`AppHandle::state`].
pub struct ManagedState<T>(T);

impl<T> ManagedState<T> {
    pub fn inner(&self) -> &T {
        &self.0
    }
}

impl<T> Deref for ManagedState<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

/// One backend notification, serialized the same way upstream sent it to the
/// web frontend. `name` is the upstream event name, for example
/// `transfer-added` or `transfer-update`.
#[derive(Debug, Clone)]
pub struct Event {
    pub name: String,
    pub payload: serde_json::Value,
}

/// Receives backend events.
///
/// The sink is called from transfer tasks, sometimes while backend locks are
/// held, so it must return immediately: hand the event to a queue and let the
/// UI drain it. It must never wait for the UI.
pub type EventSink = Arc<dyn Fn(Event) + Send + Sync>;

struct Inner {
    sink: EventSink,
    states: RwLock<HashMap<TypeId, Box<dyn Any + Send + Sync>>>,
}

/// The backend's view of its host application: where events go and where
/// shared state lives.
#[derive(Clone)]
pub struct AppHandle(Arc<Inner>);

impl AppHandle {
    pub fn new(sink: EventSink) -> Self {
        Self(Arc::new(Inner {
            sink,
            states: RwLock::new(HashMap::new()),
        }))
    }

    /// A handle that drops every event, for tests and headless tools.
    pub fn detached() -> Self {
        Self::new(Arc::new(|_| {}))
    }

    /// Registers shared state. Later registrations of the same type replace
    /// earlier ones.
    pub fn manage<T: Clone + Send + Sync + 'static>(&self, state: T) {
        self.0
            .states
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(TypeId::of::<T>(), Box::new(state));
    }

    /// Drops every managed value. Managed state can hold handles back to this
    /// `AppHandle`, so shutdown must break that cycle explicitly.
    pub fn clear_managed(&self) {
        self.0
            .states
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    pub fn try_state<T: Clone + Send + Sync + 'static>(&self) -> Option<ManagedState<T>> {
        self.0
            .states
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&TypeId::of::<T>())
            .and_then(|state| state.downcast_ref::<T>())
            .cloned()
            .map(ManagedState)
    }

    /// Returns managed state.
    ///
    /// # Panics
    ///
    /// Panics if the type was never registered, which is a wiring bug in
    /// [`crate::Core::init`].
    pub fn state<T: Clone + Send + Sync + 'static>(&self) -> ManagedState<T> {
        self.try_state()
            .unwrap_or_else(|| panic!("state {} is not managed", std::any::type_name::<T>()))
    }
}

/// Mirrors the Tauri trait that provided `emit`, so upstream call sites keep
/// compiling unchanged.
pub trait Emitter {
    fn emit<S: Serialize>(&self, event: &str, payload: S) -> Result<(), serde_json::Error>;
}

impl Emitter for AppHandle {
    fn emit<S: Serialize>(&self, event: &str, payload: S) -> Result<(), serde_json::Error> {
        let payload = serde_json::to_value(payload)?;
        (self.0.sink)(Event {
            name: event.to_string(),
            payload,
        });
        Ok(())
    }
}

/// Task spawning for backend work.
pub mod async_runtime {
    use std::future::Future;
    use std::sync::RwLock;
    use tokio::runtime::Handle;
    pub use tokio::task::JoinHandle;

    /// Identifies one [`register`] call so that only its owner can undo it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Registration(u64);

    struct Registered {
        handle: Handle,
        registration: Registration,
    }

    static RUNTIME: RwLock<Option<Registered>> = RwLock::new(None);
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

    /// Makes `handle` the runtime all backend tasks are spawned on. One
    /// backend per process is the supported arrangement; a later registration
    /// replaces an earlier one.
    pub fn register(handle: Handle) -> Registration {
        let registration = Registration(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        *RUNTIME
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Registered {
            handle,
            registration,
        });
        registration
    }

    /// Removes a registration, unless a newer one has replaced it.
    pub fn unregister(registration: Registration) {
        let mut runtime = RUNTIME
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if runtime
            .as_ref()
            .is_some_and(|current| current.registration == registration)
        {
            *runtime = None;
        }
    }

    /// Chooses where a task runs. The registered backend runtime wins so that
    /// long-lived work never lands on a short-lived caller runtime, which is
    /// how the Tauri global runtime behaved. Without a registration (unit
    /// tests of single functions) the caller's runtime is used.
    fn choose(registered: Option<Handle>, current: Option<Handle>) -> Option<Handle> {
        registered.or(current)
    }

    /// The runtime backend tasks run on.
    ///
    /// # Panics
    ///
    /// Panics when no backend is registered and the caller is not inside
    /// Tokio, which means backend work was started without [`crate::Core`].
    pub fn handle() -> Handle {
        let registered = RUNTIME
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(|registered| registered.handle.clone());
        choose(registered, Handle::try_current().ok()).expect("backend runtime is not initialized")
    }

    pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        handle().spawn(future)
    }

    pub fn spawn_blocking<F, R>(work: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        handle().spawn_blocking(work)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // These tests exercise `choose` directly instead of the process-wide
        // registration, which would leak into unrelated tests in this binary.
        #[test]
        fn the_registered_runtime_wins_over_the_callers_runtime() {
            let backend = tokio::runtime::Runtime::new().unwrap();
            let caller = tokio::runtime::Runtime::new().unwrap();

            let chosen = choose(
                Some(backend.handle().clone()),
                Some(caller.handle().clone()),
            )
            .unwrap();
            let task = chosen.spawn(async { tokio::runtime::Handle::current().id() });
            drop(caller);

            assert_eq!(backend.block_on(task).unwrap(), backend.handle().id());
        }

        #[test]
        fn the_callers_runtime_is_used_when_nothing_is_registered() {
            let caller = tokio::runtime::Runtime::new().unwrap();
            let chosen = choose(None, Some(caller.handle().clone())).unwrap();
            assert_eq!(chosen.id(), caller.handle().id());
            assert!(choose(None, None).is_none());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn emit_delivers_serialized_events_to_the_sink() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = seen.clone();
        let app = AppHandle::new(Arc::new(move |event| sink_seen.lock().unwrap().push(event)));

        app.emit("transfer-update", serde_json::json!({ "id": "a" }))
            .unwrap();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].name, "transfer-update");
        assert_eq!(seen[0].payload["id"], "a");
    }

    #[test]
    fn managed_state_is_returned_by_type() {
        let app = AppHandle::detached();
        assert!(app.try_state::<Arc<u32>>().is_none());

        app.manage(Arc::new(7u32));

        assert_eq!(**app.state::<Arc<u32>>().inner(), 7);
    }
}

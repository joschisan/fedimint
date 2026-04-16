//! Per-module state machine executor.
//!
//! Each module (and the client-level tx submission) owns a single
//! [`ModuleExecutor<S>`] parameterised by its state type. The caller
//! must hand the executor a [`Database`] it owns exclusively (typically
//! via [`Database::isolate`]); the executor persists active states in a
//! per-state-machine table and drives transitions in a typed reactor loop.
//!
//! Terminal states are simply removed from the DB; inactive state
//! history is not retained.

use std::any::Any;
use std::fmt::Debug;
use std::future::Future;
use std::hash::Hash;
use std::pin::Pin;
use std::sync::Arc;

use fedimint_core::task::{MaybeSend, MaybeSync, TaskGroup};
use fedimint_core::util::BoxFuture;
use fedimint_core::{maybe_add_send, maybe_add_send_sync, redb};
use fedimint_redb::{Database, WriteTxRef};
use futures::future::select_all;

/// A persistent state machine driven by a [`ModuleExecutor`].
///
/// Each state returns a list of possible transitions. The executor races
/// all trigger futures; when one resolves it atomically applies the
/// matching transition in a DB transaction and writes the new state.
///
/// Returning an empty `Vec` from `transitions` marks the state terminal:
/// the executor removes it from the DB and stops polling.
pub trait StateMachine:
    Debug + Clone + Eq + Hash + for<'a> redb::Key<SelfType<'a> = Self> + MaybeSend + MaybeSync + 'static
{
    /// Logical table name under which this state machine's active states are
    /// persisted in the owning [`ModuleExecutor`]'s database. Must be unique
    /// among state machine types sharing a module DB namespace.
    const TABLE_NAME: &'static str;

    /// Per-module context handed to every transition.
    type Context: Clone + MaybeSend + MaybeSync + 'static;

    /// Possible transitions out of this state.
    fn transitions(&self, ctx: &Self::Context) -> Vec<StateTransition<Self>>;
}

fn table<S: StateMachine>() -> fedimint_core::db::NativeTableDef<S, ()> {
    fedimint_core::db::NativeTableDef::new(S::TABLE_NAME)
}

/// Type-erased trigger value. The concrete type is captured inside the
/// closure stored on [`StateTransition::transition`] and recovered via
/// downcast; retries of the transition body (write conflicts) clone the
/// value from the [`Arc`].
pub type ErasedValue = Arc<maybe_add_send_sync!(dyn Any)>;

pub type TriggerFuture = Pin<Box<maybe_add_send!(dyn Future<Output = ErasedValue> + 'static)>>;

pub type TransitionFn<S> = Arc<
    maybe_add_send_sync!(dyn for<'a> Fn(&'a WriteTxRef<'_>, ErasedValue, S) -> BoxFuture<'a, S>),
>;

pub struct StateTransition<S> {
    pub trigger: TriggerFuture,
    pub transition: TransitionFn<S>,
}

impl<S: StateMachine> StateTransition<S> {
    pub fn new<V, Trigger, F>(trigger: Trigger, transition: F) -> Self
    where
        V: Clone + MaybeSend + MaybeSync + 'static,
        Trigger: Future<Output = V> + MaybeSend + 'static,
        F: for<'a> Fn(&'a WriteTxRef<'_>, V, S) -> BoxFuture<'a, S>
            + MaybeSend
            + MaybeSync
            + Clone
            + 'static,
    {
        Self {
            trigger: Box::pin(async move { Arc::new(trigger.await) as ErasedValue }),
            transition: Arc::new(move |dbtx, val, state| {
                let transition = transition.clone();
                let v: V = val
                    .downcast_ref::<V>()
                    .expect("transition value type mismatch")
                    .clone();
                Box::pin(async move { transition(dbtx, v, state).await })
            }),
        }
    }
}

// ---- Executor --------------------------------------------------------------

/// Per-module reactor driving state machines of type `S`. Cheaply
/// cloneable ([`Arc`]-backed).
#[derive(Clone)]
pub struct ModuleExecutor<S: StateMachine> {
    inner: Arc<Inner<S>>,
}

impl<S: StateMachine> Debug for ModuleExecutor<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ModuleExecutor<{}>", std::any::type_name::<S>())
    }
}

struct Inner<S: StateMachine> {
    db: Database,
    context: S::Context,
    task_group: TaskGroup,
}

impl<S: StateMachine> ModuleExecutor<S> {
    /// Create the executor. Call [`Self::start`] after all circular
    /// client references are resolved to begin driving persisted state
    /// machines.
    pub fn new(db: Database, context: S::Context, task_group: TaskGroup) -> Self {
        let inner = Arc::new(Inner {
            db,
            context,
            task_group,
        });

        Self { inner }
    }

    /// Spawn driver tasks for state machines already persisted from a
    /// previous run. Must be called exactly once per executor, after the
    /// owning client's `FinalClientIface` (and similar late-bound refs
    /// used by `S::Context`) have been resolved.
    pub async fn start(&self) {
        for state in self.inner.get_active_states().await {
            self.inner.clone().spawn_drive(state);
        }
    }

    /// Atomically insert `state` as a new active state machine. A
    /// driver task is spawned for it when the DB transaction commits.
    ///
    /// `dbtx` must be scoped to the module's DB namespace.
    pub async fn add_state_machine_dbtx(&self, dbtx: &WriteTxRef<'_>, state: S) {
        assert!(
            dbtx.insert(&table::<S>(), &state, &()).is_none(),
            "State already exists in executor"
        );

        let inner = self.inner.clone();

        dbtx.on_commit(move || {
            inner.spawn_drive(state);
        });
    }

    pub async fn get_active_states(&self) -> Vec<S> {
        self.inner.get_active_states().await
    }

    /// Like [`Self::add_state_machine_dbtx`] but does not spawn the
    /// driver task — the state will be picked up by the next call to
    /// [`Self::start`]. Used by pre-init paths (e.g. recovery) that need
    /// to seed state before the executor exists. `dbtx` must be scoped
    /// to the module's DB namespace.
    pub async fn add_state_machine_unstarted(dbtx: &WriteTxRef<'_>, state: S) {
        assert!(
            dbtx.insert(&table::<S>(), &state, &()).is_none(),
            "State already exists in executor"
        );
    }
}

impl<S: StateMachine> Inner<S> {
    async fn get_active_states(&self) -> Vec<S> {
        self.db
            .begin_read()
            .await
            .iter(&table::<S>())
            .into_iter()
            .map(|(k, ())| k)
            .collect()
    }

    async fn remove_active(&self, state: &S) {
        let tx = self.db.begin_write().await;
        tx.remove(&table::<S>(), state);
        tx.commit().await;
    }

    fn spawn_drive(self: Arc<Self>, state: S) {
        let tg = self.task_group.clone();
        tg.spawn_cancellable("sm-drive", async move { self.drive(state).await });
    }

    /// Drive one state machine from its current state to a terminal
    /// one. Each iteration: race triggers, apply the winning transition
    /// atomically, advance to the new state (or exit when terminal).
    async fn drive(self: Arc<Self>, mut state: S) {
        loop {
            let transitions = state.transitions(&self.context);

            if transitions.is_empty() {
                self.remove_active(&state).await;
                return;
            }

            let (outcome, transition) = select_all(transitions.into_iter().map(|t| {
                Box::pin(async move { (t.trigger.await, t.transition) })
                    as BoxFuture<'_, (ErasedValue, TransitionFn<S>)>
            }))
            .await
            .0;

            let tx = self.db.begin_write().await;
            let new_state = transition(&tx.as_ref(), outcome, state.clone()).await;
            tx.remove(&table::<S>(), &state);
            let next = if new_state.transitions(&self.context).is_empty() {
                None
            } else {
                tx.insert(&table::<S>(), &new_state, &());
                Some(new_state)
            };
            tx.commit().await;

            match next {
                Some(new_state) => state = new_state,
                None => return,
            }
        }
    }
}

//! Per-module state machine executor.
//!
//! Each module (and the client-level tx submission) owns a single
//! [`ModuleExecutor<S>`] parameterised by its state type. The caller
//! must hand the executor a [`Database`] it owns exclusively (typically
//! via [`Database::isolate`]); the executor persists active states in a
//! per-state-machine table keyed by a random [`SmId`] and drives
//! transitions in a typed reactor loop.
//!
//! Every state in the enum is by construction live: `transitions()` must
//! always return a non-empty list. Terminal states aren't type-representable
//! — a transition that "finishes" the SM returns `None`, and the executor
//! then removes the row. Inactive state history is not retained.

use std::any::Any;
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::future::select_all;
use picomint_core::task::TaskGroup;
use picomint_core::util::BoxFuture;
use picomint_encoding::{Decodable, Encodable};
use picomint_redb::{Database, WriteTxRef, redb};

/// Random opaque identifier assigned by the executor when a state
/// machine is first inserted. Used as the table key; the state machine
/// struct is the stored value.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Encodable, Decodable)]
pub struct SmId([u8; 16]);

picomint_redb::consensus_key!(SmId);

impl SmId {
    fn random() -> Self {
        Self(rand::random())
    }
}

/// A persistent state machine driven by a [`ModuleExecutor`].
///
/// Each state returns a list of possible transitions. The executor races
/// all trigger futures; when one resolves it atomically applies the
/// matching transition in a DB transaction and writes the new state.
///
/// Returning an empty `Vec` from `transitions` marks the state terminal:
/// the executor removes it from the DB and stops polling.
pub trait StateMachine:
    Debug + Clone + for<'a> redb::Value<SelfType<'a> = Self> + Send + Sync + 'static
{
    /// Logical table name under which this state machine's active states are
    /// persisted in the owning [`ModuleExecutor`]'s database. Must be unique
    /// among state machine types sharing a module DB namespace.
    const TABLE_NAME: &'static str;

    /// Per-module context handed to every transition.
    type Context: Clone + Send + Sync + 'static;

    /// Possible transitions out of this state.
    fn transitions(&self, ctx: &Self::Context) -> Vec<StateTransition<Self>>;
}

fn table<S: StateMachine>() -> picomint_redb::NativeTableDef<SmId, S> {
    picomint_redb::NativeTableDef::new(S::TABLE_NAME)
}

/// Type-erased trigger value. The concrete type is captured inside the
/// closure stored on [`StateTransition::transition`] and recovered via
/// downcast; retries of the transition body (write conflicts) clone the
/// value from the [`Arc`].
pub type ErasedValue = Arc<dyn Any + Send + Sync>;

pub type TriggerFuture = Pin<Box<dyn Future<Output = ErasedValue> + 'static + Send>>;

pub type TransitionFn<S> = Arc<
    dyn for<'a> Fn(&'a WriteTxRef<'_>, ErasedValue, S) -> BoxFuture<'a, Option<S>> + Send + Sync,
>;

pub struct StateTransition<S> {
    pub trigger: TriggerFuture,
    pub transition: TransitionFn<S>,
}

impl<S: StateMachine> StateTransition<S> {
    pub fn new<V, Trigger, F>(trigger: Trigger, transition: F) -> Self
    where
        V: Clone + Send + Sync + 'static,
        Trigger: Future<Output = V> + Send + 'static,
        F: for<'a> Fn(&'a WriteTxRef<'_>, V, S) -> BoxFuture<'a, Option<S>>
            + Send
            + Sync
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
        for (id, state) in self.inner.get_active_states().await {
            self.inner.clone().spawn_drive(id, state);
        }
    }

    /// Atomically insert `state` as a new active state machine under a
    /// freshly-generated [`SmId`]. A driver task is spawned for it when
    /// the DB transaction commits.
    ///
    /// `dbtx` must be scoped to the module's DB namespace.
    pub async fn add_state_machine_dbtx(&self, dbtx: &WriteTxRef<'_>, state: S) {
        let id = SmId::random();
        assert!(
            dbtx.insert(&table::<S>(), &id, &state).is_none(),
            "SmId collision"
        );

        let inner = self.inner.clone();

        dbtx.on_commit(move || {
            inner.spawn_drive(id, state);
        });
    }

    pub async fn get_active_states(&self) -> Vec<(SmId, S)> {
        self.inner.get_active_states().await
    }

    /// Like [`Self::add_state_machine_dbtx`] but does not spawn the
    /// driver task — the state will be picked up by the next call to
    /// [`Self::start`]. Used by pre-init paths (e.g. recovery) that need
    /// to seed state before the executor exists. `dbtx` must be scoped
    /// to the module's DB namespace.
    pub async fn add_state_machine_unstarted(dbtx: &WriteTxRef<'_>, state: S) {
        let id = SmId::random();
        assert!(
            dbtx.insert(&table::<S>(), &id, &state).is_none(),
            "SmId collision"
        );
    }
}

impl<S: StateMachine> Inner<S> {
    async fn get_active_states(&self) -> Vec<(SmId, S)> {
        self.db
            .begin_read()
            .await
            .iter(&table::<S>())
            .into_iter()
            .collect()
    }

    fn spawn_drive(self: Arc<Self>, id: SmId, state: S) {
        let tg = self.task_group.clone();
        tg.spawn_cancellable("sm-drive", async move { self.drive(id, state).await });
    }

    /// Drive one state machine until a transition returns `None`. Each
    /// iteration: race triggers, apply the winning transition atomically,
    /// write the new state (or delete the row if the transition terminates).
    async fn drive(self: Arc<Self>, id: SmId, mut state: S) {
        loop {
            let transitions = state.transitions(&self.context);
            assert!(
                !transitions.is_empty(),
                "StateMachine::transitions returned empty Vec — every live state must have transitions",
            );

            let (outcome, transition) = select_all(transitions.into_iter().map(|t| {
                Box::pin(async move { (t.trigger.await, t.transition) })
                    as BoxFuture<'_, (ErasedValue, TransitionFn<S>)>
            }))
            .await
            .0;

            let tx = self.db.begin_write().await;
            match transition(&tx.as_ref(), outcome, state.clone()).await {
                Some(new_state) => {
                    tx.insert(&table::<S>(), &id, &new_state);
                    tx.commit().await;
                    state = new_state;
                }
                None => {
                    tx.remove(&table::<S>(), &id);
                    tx.commit().await;
                    return;
                }
            }
        }
    }
}

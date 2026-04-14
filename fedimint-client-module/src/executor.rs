//! Per-module state machine executor.
//!
//! Each module (and the client-level tx submission) owns a single
//! [`ModuleExecutor<S>`] parameterised by its state type. The caller
//! must hand the executor a [`Database`] it owns exclusively (typically
//! via [`Database::with_prefix`]); the executor persists active states
//! at the root of that namespace and drives transitions in a typed
//! reactor loop.
//!
//! Terminal states are simply removed from the DB; inactive state
//! history is not retained.

use std::any::Any;
use std::convert::Infallible;
use std::fmt::Debug;
use std::future::Future;
use std::hash::Hash;
use std::io::{self, Read, Write};
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;

use fedimint_core::db::{
    Database, DatabaseLookup, DatabaseRecord, DatabaseTransaction,
    IDatabaseTransactionOpsCoreTyped as _,
};
use fedimint_core::encoding::{Decodable, DecodeError, Encodable};
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::task::{MaybeSend, MaybeSync, TaskGroup};
use fedimint_core::util::BoxFuture;
use fedimint_core::{maybe_add_send, maybe_add_send_sync};
use futures::future::select_all;
use futures::stream::StreamExt;

/// A persistent state machine driven by a [`ModuleExecutor`].
///
/// Each state returns a list of possible transitions. The executor races
/// all trigger futures; when one resolves it atomically applies the
/// matching transition in a DB transaction and writes the new state.
///
/// Returning an empty `Vec` from `transitions` marks the state terminal:
/// the executor removes it from the DB and stops polling.
pub trait StateMachine:
    Debug + Clone + Eq + Hash + Encodable + Decodable + MaybeSend + MaybeSync + 'static
{
    /// DB prefix byte under which this SM's active states live in the
    /// owning [`ModuleExecutor`]'s database. Must be unique among state
    /// machine types sharing a module DB.
    const DB_PREFIX: u8;

    /// Per-module context handed to every transition.
    type Context: Clone + MaybeSend + MaybeSync + 'static;

    /// Possible transitions out of this state.
    fn transitions(&self, ctx: &Self::Context) -> Vec<StateTransition<Self>>;
}

/// Type-erased trigger value. The concrete type is captured inside the
/// closure stored on [`StateTransition::transition`] and recovered via
/// downcast; retries of the transition body (autocommit conflicts) clone
/// the value from the [`Arc`].
pub type ErasedValue = Arc<maybe_add_send_sync!(dyn Any)>;

pub type TriggerFuture = Pin<Box<maybe_add_send!(dyn Future<Output = ErasedValue> + 'static)>>;

pub type TransitionFn<S> = Arc<
    maybe_add_send_sync!(
        dyn for<'a> Fn(&'a mut DatabaseTransaction<'_>, ErasedValue, S) -> BoxFuture<'a, S>
    ),
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
        F: for<'a> Fn(&'a mut DatabaseTransaction<'_>, V, S) -> BoxFuture<'a, S>
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

// ---- DB records ------------------------------------------------------------

#[derive(Debug, Clone, Eq, PartialEq, Hash, Encodable)]
pub struct ActiveStateKey<S: Encodable>(pub S);

// Decodable derive does not handle generics, so we implement manually.
impl<S: Encodable + Decodable> Decodable for ActiveStateKey<S> {
    fn consensus_decode_partial<R: Read>(
        r: &mut R,
        modules: &ModuleDecoderRegistry,
    ) -> Result<Self, DecodeError> {
        Ok(Self(S::consensus_decode_partial(r, modules)?))
    }
}

impl<S: StateMachine> DatabaseRecord for ActiveStateKey<S> {
    const DB_PREFIX: u8 = S::DB_PREFIX;
    type Key = Self;
    type Value = ();
}

/// Prefix lookup for scanning all active states.
pub struct ActiveStatePrefixAll<S>(PhantomData<S>);

impl<S> Default for ActiveStatePrefixAll<S> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<S> Debug for ActiveStatePrefixAll<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ActiveStatePrefixAll")
    }
}

impl<S> Encodable for ActiveStatePrefixAll<S> {
    fn consensus_encode<W: Write>(&self, _w: &mut W) -> Result<(), io::Error> {
        Ok(())
    }
}

impl<S: StateMachine> DatabaseLookup for ActiveStatePrefixAll<S> {
    type Record = ActiveStateKey<S>;
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
    /// `dbtx` must be the module-prefixed dbtx.
    pub async fn add_state_machine_dbtx(&self, dbtx: &mut DatabaseTransaction<'_>, state: S) {
        let key = ActiveStateKey(state.clone());

        if dbtx.get_value(&key).await.is_some() {
            panic!("State already exists in executor");
        }

        dbtx.insert_new_entry(&key, &()).await;

        let inner = self.inner.clone();

        dbtx.on_commit(move || {
            inner.spawn_drive(state);
        });
    }

    pub async fn get_active_states(&self) -> Vec<S> {
        self.inner.get_active_states().await
    }
}

impl<S: StateMachine> Inner<S> {
    async fn get_active_states(&self) -> Vec<S> {
        self.db
            .begin_transaction_nc()
            .await
            .find_by_prefix(&ActiveStatePrefixAll::<S>::default())
            .await
            .map(|(k, ())| k.0)
            .collect()
            .await
    }

    async fn remove_active(&self, state: &S) {
        self.db
            .autocommit::<_, _, Infallible>(
                |dbtx, _| {
                    Box::pin(async {
                        dbtx.remove_entry(&ActiveStateKey(state.clone())).await;
                        Ok(())
                    })
                },
                None,
            )
            .await
            .expect("autocommit retries forever");
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

            let next = self
                .db
                .autocommit::<_, _, Infallible>(
                    |dbtx, _| {
                        let state = state.clone();
                        let transition = transition.clone();
                        let outcome = outcome.clone();
                        let ctx = self.context.clone();

                        Box::pin(async move {
                            let new_state = transition(dbtx, outcome, state.clone()).await;

                            dbtx.remove_entry(&ActiveStateKey(state)).await;

                            if new_state.transitions(&ctx).is_empty() {
                                Ok(None)
                            } else {
                                dbtx.insert_entry(&ActiveStateKey(new_state.clone()), &())
                                    .await;

                                Ok(Some(new_state))
                            }
                        })
                    },
                    None,
                )
                .await
                .expect("autocommit retries forever");

            match next {
                Some(new_state) => state = new_state,
                None => return,
            }
        }
    }
}

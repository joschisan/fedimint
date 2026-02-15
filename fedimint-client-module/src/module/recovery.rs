use std::any::Any;
use std::fmt::{self, Debug};

use fedimint_core::core::{IntoDynInstance, ModuleInstanceId, ModuleKind};
use fedimint_core::encoding::{Decodable, DynEncodable, Encodable};
use fedimint_core::task::{MaybeSend, MaybeSync};
use fedimint_core::{
    maybe_add_send_sync, module_plugin_dyn_newtype_clone_passthrough,
    module_plugin_dyn_newtype_define, module_plugin_dyn_newtype_encode_decode,
    module_plugin_dyn_newtype_eq_passthrough,
};
use serde::{Deserialize, Serialize};

pub trait IModuleBackup: Debug + DynEncodable {
    fn as_any(&self) -> &(maybe_add_send_sync!(dyn Any));
    fn module_kind(&self) -> Option<ModuleKind>;
    fn clone(&self, instance_id: ModuleInstanceId) -> DynModuleBackup;
    fn erased_eq_no_instance_id(&self, other: &DynModuleBackup) -> bool;
}

pub trait ModuleBackup:
    std::fmt::Debug
    + IntoDynInstance<DynType = DynModuleBackup>
    + std::cmp::PartialEq
    + DynEncodable
    + Decodable
    + Clone
    + MaybeSend
    + MaybeSync
    + 'static
{
    const KIND: Option<ModuleKind>;
}

impl IModuleBackup for ::fedimint_core::core::DynUnknown {
    fn as_any(&self) -> &(maybe_add_send_sync!(dyn Any)) {
        self
    }

    fn module_kind(&self) -> Option<ModuleKind> {
        None
    }

    fn clone(&self, instance_id: ::fedimint_core::core::ModuleInstanceId) -> DynModuleBackup {
        DynModuleBackup::from_typed(instance_id, <Self as Clone>::clone(self))
    }

    fn erased_eq_no_instance_id(&self, other: &DynModuleBackup) -> bool {
        let other: &Self = other
            .as_any()
            .downcast_ref()
            .expect("Type is ensured in previous step");

        self == other
    }
}

impl<T> IModuleBackup for T
where
    T: ModuleBackup,
{
    fn as_any(&self) -> &(maybe_add_send_sync!(dyn Any)) {
        self
    }

    fn module_kind(&self) -> Option<ModuleKind> {
        T::KIND
    }

    fn clone(&self, instance_id: ::fedimint_core::core::ModuleInstanceId) -> DynModuleBackup {
        DynModuleBackup::from_typed(instance_id, <Self as Clone>::clone(self))
    }

    fn erased_eq_no_instance_id(&self, other: &DynModuleBackup) -> bool {
        let other: &Self = other
            .as_any()
            .downcast_ref()
            .expect("Type is ensured in previous step");

        self == other
    }
}

module_plugin_dyn_newtype_define! {
    pub DynModuleBackup(Box<IModuleBackup>)
}

module_plugin_dyn_newtype_encode_decode!(DynModuleBackup);

module_plugin_dyn_newtype_clone_passthrough!(DynModuleBackup);

module_plugin_dyn_newtype_eq_passthrough!(DynModuleBackup);

/// A backup type for modules without a backup implementation. The default
/// variant allows implementing a backup strategy for the module later on by
/// copying this enum into the module and adding a second variant to it.
#[derive(Clone, PartialEq, Eq, Debug, Encodable, Decodable)]
pub enum NoModuleBackup {
    NoModuleBackup,
    #[encodable_default]
    Default {
        variant: u64,
        bytes: Vec<u8>,
    },
}

impl ModuleBackup for NoModuleBackup {
    const KIND: Option<ModuleKind> = None;
}

impl IntoDynInstance for NoModuleBackup {
    type DynType = DynModuleBackup;

    fn into_dyn(self, instance_id: ModuleInstanceId) -> Self::DynType {
        DynModuleBackup::from_typed(instance_id, self)
    }
}

/// Progress of the recovery as `complete` out of `total` items.
#[derive(Debug, Copy, Clone, Encodable, Decodable, Serialize, Deserialize)]
pub struct RecoveryProgress {
    pub complete: u32,
    pub total: u32,
}

impl RecoveryProgress {
    pub fn new(complete: u32, total: u32) -> Self {
        Self { complete, total }
    }

    pub fn is_done(self) -> bool {
        self.total <= self.complete
    }

    pub fn to_fraction(self) -> f64 {
        f64::from(self.complete) / f64::from(self.total)
    }
}

impl fmt::Display for RecoveryProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("{}/{}", self.complete, self.total))
    }
}

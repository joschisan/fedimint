/// Implements the necessary traits for all configuration related types of a
/// `FederationServer` module.
#[macro_export]
macro_rules! plugin_types_trait_impl_config {
    ($common_gen:ty, $cfg:ty, $cfg_private:ty, $cfg_consensus:ty, $cfg_client:ty) => {
        impl picomint_core::config::TypedServerModuleConsensusConfig for $cfg_consensus {
            fn kind(&self) -> picomint_core::core::ModuleKind {
                <$common_gen as picomint_core::module::CommonModuleInit>::KIND
            }

            fn version(&self) -> picomint_core::module::ModuleConsensusVersion {
                <$common_gen as picomint_core::module::CommonModuleInit>::CONSENSUS_VERSION
            }
        }

        impl picomint_core::config::TypedServerModuleConfig for $cfg {
            type Private = $cfg_private;
            type Consensus = $cfg_consensus;

            fn from_parts(private: Self::Private, consensus: Self::Consensus) -> Self {
                Self { private, consensus }
            }

            fn to_parts(self) -> (ModuleKind, Self::Private, Self::Consensus) {
                (
                    <$common_gen as picomint_core::module::CommonModuleInit>::KIND,
                    self.private,
                    self.consensus,
                )
            }
        }
    };
}

/// Implements the necessary traits for all associated types of a
/// `FederationServer` module.
#[macro_export]
macro_rules! plugin_types_trait_impl_common {
    ($kind:expr_2021, $types:ty, $client_config:ty, $input:ty, $output:ty, $ci:ty, $input_error:ty, $output_error:ty) => {
        impl picomint_core::module::ModuleCommon for $types {
            type ClientConfig = $client_config;
            type Input = $input;
            type Output = $output;
            type ConsensusItem = $ci;
            type InputError = $input_error;
            type OutputError = $output_error;
        }
    };
}

// TODO: use concat_ident for error name once it lands in stable, see https://github.com/rust-lang/rust/issues/29599
/// Macro for defining module associated types.
///
/// Wraps a type into an enum with a default variant, this allows to add new
/// versions of the type in the future. Depending on context unknown versions
/// may be ignored or lead to errors. E.g. the client might just ignore an
/// unknown input version since it cannot originate from itself while the server
/// would reject it for not being able to validate its correctness.
///
/// Adding extensibility this way is a last line of defense against breaking
/// changes, most often other ways of introducing new functionality should be
/// preferred (e.g. new module versions, pure client-side changes, …).
#[macro_export]
macro_rules! extensible_associated_module_type {
    ($name:ident, $name_v0:ident) => {
        #[derive(
            Clone,
            Eq,
            PartialEq,
            Hash,
            serde::Deserialize,
            serde::Serialize,
            picomint_core::encoding::Encodable,
            picomint_core::encoding::Decodable,
        )]
        pub enum $name {
            V0($name_v0),
        }

        impl $name {
            pub fn as_v0_ref(&self) -> &$name_v0 {
                let $name::V0(v0) = self;
                v0
            }
        }

        impl std::convert::From<$name_v0> for $name {
            fn from(v: $name_v0) -> Self {
                Self::V0(v)
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                let $name::V0(v0) = self;
                std::fmt::Debug::fmt(v0, f)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                let $name::V0(inner) = self;
                std::fmt::Display::fmt(inner, f)
            }
        }
    };
}

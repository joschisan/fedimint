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

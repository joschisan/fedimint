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


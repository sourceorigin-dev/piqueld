//! Field edits: one method per field endpoint, wrapping its generated
//! operation, and `Client::edit_application`, all built from
//! `piqueld_core::field_endpoints!`.
use crate::{Client, ClientError, SavedApplication, client::EnvelopedCall};
use piqueld_core::{
    edit::{
        ApplicationEdit, BoolValue, CountValue, CpuValue, EditOptions, EnvironmentValue,
        ExportsValue, FilesValue, GeneratedSecretsValue, HealthValue, ImportsValue, JobsValue,
        MemoryValue, MountsValue, OptionalStringValue, ReplicasValue, RepositorySyncValue,
        RepositoryValue, ResourcesValue, RollbackValue, RolloutValue, RoutesValue, SecondsValue,
        SecretsValue, SecurityValue, ServiceEdit, ServiceGeneral, ServiceProcess, SourceValue,
        StringValue, StringsValue, TemplateValue, TemplatesValue, VariablesValue, VisibilityValue,
        VolumesValue,
    },
    manifest::{Mount, Service, ServiceFile, Volume},
};

// The generated client owns paths, encoding, request bodies, and response
// types. For each entry of `piqueld_core::field_endpoints!` this builds a
// `Client` method taking the path parameters, the body if any, and
// `EditOptions` (deploy, revision, force), and `edit_application`, which
// matches an edit against every entry's edit to send it through its endpoint.
// Like every other wrapper, a method sends `force` only when set; the server
// reads absence as false.
macro_rules! edit_methods {
    ($($(#[$doc:meta])* $name:ident: $method:ident $path:literal ($($param:ident = $label:literal),*) $body:tt => { $($edit:tt)* };)*) => {
        $(edit_methods!(@method $name ($($param),*) $body);)*

        impl Client {
            /// Sends a typed change through its field endpoint: the first
            /// entry of `piqueld_core::field_endpoints!` whose edit matches.
            /// # Errors
            /// Returns transport, validation, revision, or resource errors.
            pub async fn edit_application(
                &self,
                id: &str,
                edit: &ApplicationEdit,
                options: &EditOptions,
            ) -> Result<SavedApplication, ClientError> {
                match edit.clone() {
                    $($($edit)* => edit_methods!(@send self, id, options, $name ($($param),*) $body),)*
                }
            }
        }
    };
    // Without a body.
    (@method $name:ident ($($param:ident),*) []) => {
        edit_methods!(@method $name ($($param),*) ());
    };
    // A `{ "value": ... }` body.
    (@method $name:ident ($($param:ident),*) [$body:ident { $value:ident }]) => {
        edit_methods!(@method $name ($($param),*) (request: &$body));
    };
    // A boxed whole body, taken unboxed as the generated method does.
    (@method $name:ident ($($param:ident),*) [$binding:ident: Box<$body:ty>]) => {
        edit_methods!(@method $name ($($param),*) (request: &$body));
    };
    // A whole body.
    (@method $name:ident ($($param:ident),*) [$binding:ident: $body:ty]) => {
        edit_methods!(@method $name ($($param),*) (request: &$body));
    };
    (@method $name:ident ($($param:ident),*) ($($request:ident: $body:ty)?)) => {
        impl Client {
            #[doc = concat!("Calls the generated `", stringify!($name), "` endpoint.")]
            /// # Errors
            /// Returns transport, validation, revision, or resource errors.
            pub async fn $name(&self, id: &str, $($param: &str,)* $($request: $body,)? options: &EditOptions) -> Result<SavedApplication, ClientError> {
                self.generated.$name(id, $($param,)* Some(options.deploy), options.expected_generation, options.force.then_some(true), None $(, $request)?).data().await
            }
        }
    };
    (@send $client:ident, $id:ident, $options:ident, $name:ident ($($param:ident),*) []) => {
        $client.$name($id, $(&$param,)* $options).await
    };
    (@send $client:ident, $id:ident, $options:ident, $name:ident ($($param:ident),*) [$body:ident { $value:ident }]) => {
        $client.$name($id, $(&$param,)* &$body { $value }, $options).await
    };
    (@send $client:ident, $id:ident, $options:ident, $name:ident ($($param:ident),*) [$binding:ident: $body:ty]) => {
        $client.$name($id, $(&$param,)* &$binding, $options).await
    };
}
piqueld_core::field_endpoints!(edit_methods);

impl Client {
    /// Creates an empty application, requiring the name to be absent.
    /// # Errors
    /// Returns transport, validation, or name collision errors.
    pub async fn create_application(
        &self,
        name: &str,
        deploy: bool,
    ) -> Result<SavedApplication, ClientError> {
        self.generated
            .create_application(Some(deploy), None, &StringValue { value: name.into() })
            .data()
            .await
    }
}

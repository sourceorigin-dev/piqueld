//! Generates or checks the `OpenAPI` specification, its Rust client bindings
//! and operation paths, the standalone application manifest and daemon
//! configuration JSON Schemas, and the documentation's settings references
//! rendered from those schemas. `piquelctl`'s `cli_reference` example renders
//! the command reference; it depends on the client this generates, so it runs
//! as a separate step.

mod settings_reference;

use anyhow::{Context, Result, anyhow, bail, ensure};
use heck::ToSnakeCase;
use piqueld::api::http::openapi_document;
use piqueld_config::DaemonConfig;
use piqueld_core::manifest::ApplicationManifest;
use piqueld_generated::Artifacts;
use serde::Deserialize;
use serde_json::{Map, Value};
use settings_reference::SettingsPage;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs,
    io::Write,
    process::{Command, Stdio},
};

/// Client generation settings read from `tools/client-codegen/config.json`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    replacements: Vec<Replacement>,
}

/// Maps an `OpenAPI` component schema onto an existing Rust type, so the client
/// reuses `piqueld-core` types instead of generating duplicates.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Replacement {
    /// Component schema name in the `OpenAPI` document.
    schema: String,
    /// Type name Progenitor would otherwise generate for the schema.
    name: String,
    /// Fully qualified Rust type used in its place.
    rust_type: String,
}

/// Renders every artifact before writing or checking any of them.
struct Generator {
    /// Where the artifacts go, and whether to check them instead.
    artifacts: Artifacts,
}

impl Generator {
    /// Renders all the artifacts, then writes them, or in check mode compares
    /// them against the checked-in files and fails listing any that are stale
    /// or missing.
    ///
    /// Every `OpenAPI` path must live under `/api/`.
    fn run(&self) -> Result<()> {
        let document = openapi_document();
        let paths = document
            .get("paths")
            .and_then(Value::as_object)
            .context("OpenAPI document has no paths object")?;
        for path in paths.keys() {
            ensure!(
                path.starts_with("/api/"),
                "OpenAPI path is outside the API namespace: {path}"
            );
        }

        let config: Config = serde_json::from_slice(
            &fs::read(
                self.artifacts
                    .root()
                    .join("tools/client-codegen/config.json"),
            )
            .context("read Progenitor configuration")?,
        )
        .context("parse Progenitor configuration")?;
        let operation_paths = Self::generate_paths(paths)?;
        let client = self.generate_client(document.clone(), &config)?;
        let document = format!("{}\n", serde_json::to_string_pretty(&document)?);
        let manifest_schema = ApplicationManifest::json_schema();
        let manifest_settings = SettingsPage::MANIFEST.render(&manifest_schema)?;
        let manifest_schema = format!("{}\n", serde_json::to_string_pretty(&manifest_schema)?);
        let config_schema = DaemonConfig::json_schema();
        let config_settings = SettingsPage::CONFIG.render(&config_schema)?;
        let config_schema = format!("{}\n", serde_json::to_string_pretty(&config_schema)?);

        self.artifacts.sync([
            ("docs/openapi-v1.json", document),
            ("docs/application-manifest.schema.json", manifest_schema),
            ("docs/piqueld-config.schema.json", config_schema),
            ("crates/piqueld-client/src/generated.rs", client),
            (
                "crates/piqueld-client/src/paths.rs",
                self.rustfmt(&operation_paths)?,
            ),
            (SettingsPage::MANIFEST.path, manifest_settings),
            (SettingsPage::CONFIG.path, config_settings),
        ])?;
        Ok(())
    }

    /// Generates the Progenitor client source for `crates/piqueld-client`.
    ///
    /// The client wraps `crate::client::ClientState`, applies the configured type
    /// replacements, routes response decoding through `crate::client::decode_response`,
    /// and is formatted with the repository's `rustfmt` configuration.
    fn generate_client(&self, mut document: Value, config: &Config) -> Result<String> {
        Self::prepare_client_document(&mut document, config)?;
        let spec = serde_json::from_value(document).context("parse OpenAPI 3.0 document")?;

        let mut settings = progenitor::GenerationSettings::default();
        let inner = "crate::client::ClientState"
            .parse()
            .map_err(|error| anyhow!("parse generated client state type: {error}"))?;
        settings.with_inner_type(inner);
        for replacement in &config.replacements {
            settings.with_replacement(
                &replacement.name,
                &replacement.rust_type,
                std::iter::empty(),
            );
        }

        let tokens = progenitor::Generator::new(&settings)
            .generate_tokens(&spec)
            .context("generate Progenitor client")?;
        let syntax = syn::parse2(tokens).context("parse generated Rust")?;
        // The generated operations are private implementation details, and
        // Progenitor's indented prose is otherwise interpreted as doctest code.
        let source = prettyplease::unparse(&syntax).replace("/**", "/*");
        let decoder = "ResponseValue::from_response(response).await";
        ensure!(
            source.contains(decoder),
            "Progenitor response decoder shape changed"
        );
        let source = source.replace(decoder, "crate::client::decode_response(response).await");
        self.rustfmt(&format!(
            r#"// Generated by Progenitor. Run `just generate`; do not edit.

#![expect(
    clippy::match_same_arms,
    clippy::struct_field_names,
    clippy::unnecessary_trailing_comma,
    reason = "Progenitor emits repeated status arms, mechanical field names, and trailing commas"
)]
#![expect(
    dead_code,
    reason = "Progenitor emits ConversionError even though all schemas use shared types"
)]
#![expect(
    private_interfaces,
    reason = "generated constructors take the crate-private ClientState inside this private module"
)]
#![expect(
    clippy::allow_attributes,
    clippy::allow_attributes_without_reason,
    reason = "Progenitor marks its own exceptions with plain `#[allow]`"
)]

{source}"#
        ))
    }

    /// Generates the source of `piqueld_client::paths` from the document's
    /// `paths`: one function per operation, named after its operation ID as
    /// the generated client's methods are, that builds the operation's path
    /// with its path parameters percent-encoded as the client encodes them.
    /// It reads the complete document, so it also covers the operations
    /// Progenitor's copy drops, such as the exec WebSocket.
    fn generate_paths(paths: &Map<String, Value>) -> Result<String> {
        let mut functions = BTreeMap::new();
        for (path, item) in paths {
            let operations = item
                .as_object()
                .with_context(|| format!("OpenAPI path {path} is not an object"))?;
            for (method, operation) in operations {
                let Some(operation) = operation.as_object() else {
                    continue;
                };
                let id = operation
                    .get("operationId")
                    .and_then(Value::as_str)
                    .with_context(|| format!("{method} {path} has no operation ID"))?;
                // Progenitor names its methods with heck's snake case too.
                let name = id.to_snake_case();
                let mut template = String::new();
                let mut parameters = Vec::new();
                let mut arguments = String::new();
                // Placeholders are the odd parts between braces.
                for (index, part) in path.split(['{', '}']).enumerate() {
                    if index % 2 == 0 {
                        template.push_str(part);
                        continue;
                    }
                    let format = operation
                        .get("parameters")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .find(|parameter| {
                            parameter.get("in").and_then(Value::as_str) == Some("path")
                                && parameter.get("name").and_then(Value::as_str) == Some(part)
                        })
                        .and_then(|parameter| parameter.get("schema"))
                        .map(|schema| {
                            (
                                schema.get("type").and_then(Value::as_str),
                                schema.get("format").and_then(Value::as_str),
                            )
                        });
                    // Strings are percent-encoded; integers need no encoding.
                    let kind = match format {
                        Some((Some("string"), _)) => {
                            write!(arguments, ", {part} = encode_path({part})")?;
                            "&str"
                        }
                        Some((Some("integer"), Some("int64"))) => "i64",
                        _ => bail!("{id}'s path parameter {part} has an unsupported type"),
                    };
                    write!(template, "{{{part}}}")?;
                    parameters.push(format!("{part}: {kind}"));
                }
                let body = if parameters.is_empty() {
                    format!("String::from({template:?})")
                } else {
                    format!("format!({template:?}{arguments})")
                };
                let function = format!(
                    "/// `{} {path}`\n#[must_use]\npub fn {name}({}) -> String {{ {body} }}\n\n",
                    method.to_uppercase(),
                    parameters.join(", ")
                );
                ensure!(
                    functions.insert(name, function).is_none(),
                    "two operations are named {id}"
                );
            }
        }
        Ok(format!(
            "// Generated from the OpenAPI document. Run `just generate`; do not edit.\n\n\
             use progenitor_client::encode_path;\n\n{}",
            functions.into_values().collect::<String>()
        ))
    }

    /// Formats Rust source through `rustfmt` over stdin and stdout.
    fn rustfmt(&self, source: &str) -> Result<String> {
        let mut child = Command::new("rustfmt")
            .args(["--edition", "2024", "--emit", "stdout", "--config-path"])
            .arg(self.artifacts.root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("start rustfmt for generated client")?;
        child
            .stdin
            .take()
            .context("open rustfmt stdin")?
            .write_all(source.as_bytes())
            .context("write generated client to rustfmt")?;
        let output = child.wait_with_output().context("wait for rustfmt")?;
        ensure!(
            output.status.success(),
            "rustfmt failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).context("rustfmt returned non-UTF-8 output")
    }

    /// Simplifies the document into the subset Progenitor handles well.
    ///
    /// 1. Drops the readiness endpoint's 403 response, which Progenitor cannot
    ///    type alongside its readiness-body 503.
    /// 2. Keeps only the `application/json` request body when several are offered.
    /// 3. Strips parameter constraints Progenitor cannot express (`nullable`, string
    ///    length and pattern, numeric bounds), mapping non-negative `int32`/`int64`
    ///    parameters to `uint32`/`uint64`.
    /// 4. Drops every component schema except the replaced ones, which must exist,
    ///    so shared models always come from `piqueld-core`.
    fn prepare_client_document(document: &mut Value, config: &Config) -> Result<()> {
        let paths = document
            .get_mut("paths")
            .and_then(Value::as_object_mut)
            .context("OpenAPI document has no paths object")?;
        // Generated WebSocket methods return `reqwest::Upgraded`, which WASM
        // builds lack, so the exec WebSocket is written by hand in
        // `piqueld_client::exec` for native targets.
        ensure!(
            paths.remove("/api/v1/environments/{id}/exec").is_some(),
            "missing exec operation"
        );
        // Readiness uses a typed readiness body for 503. Progenitor cannot
        // generate two different error-body types for one operation; preserve
        // its existing readiness decoder and let middleware errors use the
        // unexpected-response path. The published contract retains both.
        if let Some(responses) = paths
            .get_mut("/api/v1/system/readiness")
            .and_then(|path| path.get_mut("get"))
            .and_then(|operation| operation.get_mut("responses"))
            .and_then(Value::as_object_mut)
        {
            responses.remove("403");
        }
        for item in paths.values_mut().filter_map(Value::as_object_mut) {
            for operation in item.values_mut().filter_map(Value::as_object_mut) {
                if let Some(content) = operation
                    .get_mut("requestBody")
                    .and_then(|body| body.get_mut("content"))
                    .and_then(Value::as_object_mut)
                    && let Some(json) = content.remove("application/json")
                {
                    content.clear();
                    content.insert("application/json".into(), json);
                }
                if let Some(parameters) = operation
                    .get_mut("parameters")
                    .and_then(Value::as_array_mut)
                {
                    for parameter in parameters.iter_mut().filter_map(Value::as_object_mut) {
                        let Some(schema) =
                            parameter.get_mut("schema").and_then(Value::as_object_mut)
                        else {
                            continue;
                        };
                        schema.remove("nullable");
                        if schema.get("type").and_then(Value::as_str) == Some("string") {
                            schema.remove("minLength");
                            schema.remove("maxLength");
                            schema.remove("pattern");
                        }
                        if schema.get("type").and_then(Value::as_str) == Some("integer")
                            && schema
                                .get("minimum")
                                .and_then(Value::as_f64)
                                .is_some_and(|minimum| minimum >= 0.0)
                        {
                            let format = match schema.get("format").and_then(Value::as_str) {
                                Some("int32") => Some("uint32"),
                                Some("int64") => Some("uint64"),
                                _ => None,
                            };
                            if let Some(format) = format {
                                schema.insert("format".into(), Value::String(format.into()));
                            }
                        }
                        schema.remove("minimum");
                        schema.remove("maximum");
                    }
                }
            }
        }

        let schemas = document
            .get_mut("components")
            .and_then(|components| components.get_mut("schemas"))
            .and_then(Value::as_object_mut)
            .context("OpenAPI document has no component schemas")?;
        let retained = config
            .replacements
            .iter()
            .map(|replacement| replacement.schema.as_str())
            .collect::<BTreeSet<_>>();
        for schema in &retained {
            ensure!(
                schemas.contains_key(*schema),
                "missing mapped schema {schema}"
            );
        }
        schemas.retain(|name, _| retained.contains(name.as_str()));
        Ok(())
    }
}

fn main() -> Result<()> {
    Generator {
        artifacts: Artifacts::from_args()?,
    }
    .run()
}

//! Editing the secrets the daemon generates for an application.

use super::{Declarations, EditFlags};
use crate::{commands::resolve_application, error::Result, output::Console};
use clap::{Args, Subcommand, builder::TypedValueParser};
use piqueld_client::{
    Client, SecretDeclaration, SecretEncoding, SecretGenerator, edit::ApplicationEdit,
};

/// Parses `hex` or `base64url`, listing them with their help in help and
/// completions.
fn secret_encoding() -> impl TypedValueParser<Value = SecretEncoding> {
    crate::cli::one_of!(SecretEncoding::ALL, SecretEncoding::parse, |encoding| {
        match encoding {
            SecretEncoding::Hex => "Lowercase hexadecimal, two characters per byte.",
            SecretEncoding::Base64url => "URL-safe base64 without padding.",
        }
    })
}
#[derive(Debug, Args)]
pub(crate) struct GeneratedSecretTarget {
    /// Application name or stable ID.
    app: String,
    /// Secret name, as services and jobs mount it.
    name: String,
    #[command(flatten)]
    flags: EditFlags,
}
#[derive(Debug, Args)]
pub(crate) struct SetGeneratedSecretArgs {
    #[command(flatten)]
    target: GeneratedSecretTarget,
    #[command(flatten)]
    generator: Generator,
}
// How the value is generated: exactly one of `--bytes` or `--rsa-bits`.
#[derive(Debug, Args)]
#[command(group(clap::ArgGroup::new("generator").required(true).args(["bytes", "rsa_bits"])))]
struct GeneratorFlags {
    /// Generate this many random bytes (16–512), encoded as text.
    #[arg(long)]
    bytes: Option<u16>,
    /// Text encoding of the random bytes.
    ///
    /// By default, `hex`.
    #[arg(long, conflicts_with = "rsa_bits", value_parser = secret_encoding())]
    encoding: Option<SecretEncoding>,
    /// Generate an RSA private key of this many bits (2048, 3072, or 4096) in PKCS#8 PEM.
    #[arg(long)]
    rsa_bits: Option<u16>,
}
impl GeneratorFlags {
    /// The generator the flags select.
    fn choice(self) -> Option<Generator> {
        let random = self.bytes.map(|bytes| SecretGenerator::Random {
            bytes,
            encoding: self.encoding.unwrap_or_default(),
        });
        self.rsa_bits
            .map(|bits| SecretGenerator::Rsa { bits })
            .or(random)
            .map(Generator)
    }
}
/// The generator `generated-secret set` declares.
#[derive(Debug)]
struct Generator(SecretGenerator);
crate::cli::choice_args!(Generator, GeneratorFlags, GeneratorFlags::choice);
#[derive(Debug, Subcommand)]
pub(crate) enum GeneratedSecretCommand {
    /// List the secrets the application declares for piqueld to generate, as
    /// saved.
    ///
    /// `env secret list` shows the values generated in each environment.
    List {
        /// Application name or stable ID.
        app: String,
    },
    /// Declare a secret piqueld generates, or replace how it is generated.
    ///
    /// Give exactly one of `--bytes` or `--rsa-bits`. Values already
    /// generated are kept; `env secret regenerate` replaces them.
    Set(SetGeneratedSecretArgs),
    /// Remove a declaration.
    ///
    /// Values already generated are kept. Removing a name the application
    /// doesn't declare fails.
    Remove(GeneratedSecretTarget),
}
impl GeneratedSecretCommand {
    /// Lists saved declarations, or edits them client-side and saves the whole
    /// list, like exports: setting a declared name replaces it in place.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let (target, secret) = match self {
            Self::List { app } => {
                let application = resolve_application(client, app).await?;
                return console.emit(&application.application.to_manifest().spec.secrets);
            }
            Self::Set(args) => (
                &args.target,
                Some(SecretDeclaration {
                    name: args.target.name.clone(),
                    generate: args.generator.0.clone(),
                }),
            ),
            Self::Remove(target) => (target, None),
        };
        let current = resolve_application(client, &target.app).await?;
        let mut secrets = current.application.to_manifest().spec.secrets;
        secrets.set_or_remove(
            |secret| secret.name == target.name,
            secret,
            || format!("secret {:?} is not declared", target.name),
        )?;
        let edit = ApplicationEdit::GeneratedSecrets(secrets);
        target
            .flags
            .save_loaded(client, console, &current, &edit)
            .await
    }
}

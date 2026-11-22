//! Editable declarations of the secrets piqueld generates, `spec.secrets`.

use piqueld_client::{ApplicationManifest, SecretDeclaration, SecretEncoding, SecretGenerator};

/// One editable generated-secret declaration. `size` is the number of random
/// bytes, or the RSA key's bits, as typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclarationDraft {
    /// The secret's name.
    pub name: String,
    /// Whether it generates an RSA key rather than random bytes.
    pub rsa: bool,
    /// The number of bytes, or bits, as typed.
    pub size: String,
    /// The encoding of random bytes; unused for RSA keys.
    pub encoding: SecretEncoding,
}

impl Default for DeclarationDraft {
    fn default() -> Self {
        Self::of_kind(false)
    }
}

impl DeclarationDraft {
    /// A new random (or RSA) declaration with that generator's usual size.
    fn of_kind(rsa: bool) -> Self {
        Self {
            name: String::new(),
            rsa,
            size: if rsa { "3072" } else { "32" }.into(),
            encoding: SecretEncoding::default(),
        }
    }

    /// Switches to the random (or RSA) generator, starting again from its
    /// usual size but keeping the name; choosing the current one keeps the
    /// draft.
    pub fn set_kind(&mut self, rsa: bool) {
        if self.rsa != rsa {
            let name = std::mem::take(&mut self.name);
            *self = Self {
                name,
                ..Self::of_kind(rsa)
            };
        }
    }

    /// The manifest's declarations as editable rows.
    #[must_use]
    pub fn rows(manifest: &ApplicationManifest) -> Vec<Self> {
        manifest
            .spec
            .secrets
            .iter()
            .map(|secret| match secret.generate {
                SecretGenerator::Random { bytes, encoding } => Self {
                    name: secret.name.clone(),
                    rsa: false,
                    size: bytes.to_string(),
                    encoding,
                },
                SecretGenerator::Rsa { bits } => Self {
                    name: secret.name.clone(),
                    rsa: true,
                    size: bits.to_string(),
                    encoding: SecretEncoding::default(),
                },
            })
            .collect()
    }

    /// The declaration; manifest validation checks its size's range.
    ///
    /// # Errors
    ///
    /// Returns a message naming the secret when its size does not parse.
    #[expect(
        clippy::map_err_ignore,
        reason = "the form states what to enter, which a parse error cannot improve on"
    )]
    pub fn declaration(self) -> Result<SecretDeclaration, String> {
        let size = self
            .size
            .parse()
            .map_err(|_| format!("The size of {} must be a whole number", self.name))?;
        Ok(SecretDeclaration {
            name: self.name,
            generate: if self.rsa {
                SecretGenerator::Rsa { bits: size }
            } else {
                SecretGenerator::Random {
                    bytes: size,
                    encoding: self.encoding,
                }
            },
        })
    }
}

#[cfg(test)]
mod tests;

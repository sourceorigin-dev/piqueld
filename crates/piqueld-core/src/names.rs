//! Validated names and shared scaffolding for string-backed types.

/// Declares a `String` newtype plus a unit error type, with identical
/// construction, wire-format and standard-trait implementations. The validator
/// stays with the owning type: outside the declaring module the only way to
/// build the newtype is `parse` (also used by `Deserialize` and `FromStr`), so
/// every value has passed `$validate`. Doc attributes on the newtype feed the
/// generated `OpenAPI` schema.
///
/// ```text
/// validated_string!(
///     /// Docs for the newtype.
///     Name, NameError, "error message", |value: &str| !value.is_empty()
/// );
/// ```
macro_rules! validated_string {
    ($(#[$meta:meta])* $name:ident, $error:ident, $message:expr, $validate:expr) => {
        #[doc = concat!("Invalid `", stringify!($name), "` input.")]
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct $error;

        impl std::fmt::Display for $error {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str($message)
            }
        }

        impl std::error::Error for $error {}

        #[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Serialize, utoipa::ToSchema)]
        $(#[$meta])*
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Parses a value while enforcing this type's invariant.
            ///
            /// # Errors
            #[doc = concat!("Returns [`", stringify!($error), "`] for invalid input: ", $message, ".")]
            pub fn parse(value: impl Into<String>) -> Result<Self, $error> {
                let value = value.into();
                if ($validate)(&value) {
                    Ok(Self(value))
                } else {
                    Err($error)
                }
            }

            /// Returns the validated wire representation.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = <String as serde::Deserialize>::deserialize(deserializer)?;
                Self::parse(value).map_err(serde::de::Error::custom)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        /// Formats as its quoted value, like a `str`, since messages quote
        /// values with `{:?}`.
        impl std::fmt::Debug for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Debug::fmt(self.as_str(), formatter)
            }
        }

        impl std::str::FromStr for $name {
            type Err = $error;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                self.as_str()
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}
pub(crate) use validated_string;

/// Declares a logical name: a `validated_string!` checked by
/// `valid_logical_name`, whose error message starts with `$subject`, such as
/// "service names".
macro_rules! logical_name {
    ($(#[$meta:meta])* $name:ident, $error:ident, $subject:literal) => {
        $crate::names::validated_string!(
            $(#[$meta])*
            $name, $error,
            concat!(
                $subject,
                " must be 1-63 lowercase letters, digits, or hyphens, start with a letter, and end with a letter or digit"
            ),
            $crate::resource::valid_logical_name
        );
    };
}

/// Declares a fieldless enum whose variants each have one stable string form,
/// from a single `Variant => "form"` table, with `ALL`, `as_str`, `parse` and
/// `Display`. A leading `#[serde]` also derives `Deserialize`, `Serialize` and
/// `ToSchema`, renaming each variant to its form. Forms are matched as `tt`,
/// not `literal`: `ToSchema` ignores a rename whose value arrives as a
/// `literal` fragment.
///
/// ```text
/// string_enum! {
///     #[serde]
///     /// Docs for the enum.
///     pub enum Outcome {
///         /// Docs for the variant.
///         Allowed => "allowed",
///     }
/// }
/// ```
macro_rules! string_enum {
    (
        #[serde]
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $($(#[$variant_meta:meta])* $variant:ident => $value:tt),+ $(,)?
        }
    ) => {
        $crate::names::string_enum! {
            $(#[$meta])*
            #[derive(serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
            $vis enum $name {
                $($(#[$variant_meta])* #[serde(rename = $value)] $variant => $value),+
            }
        }
    };
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $($(#[$variant_meta:meta])* $variant:ident => $value:tt),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        $vis enum $name {
            $($(#[$variant_meta])* $variant),+
        }

        impl $name {
            /// Every value, in declaration order.
            pub const ALL: [Self; [$($value),+].len()] = [$(Self::$variant),+];

            /// Stable string form, as stored, sent, and shown.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $value),+
                }
            }

            /// Parses a stable string form, rejecting unknown ones.
            #[must_use]
            pub fn parse(value: &str) -> Option<Self> {
                match value {
                    $($value => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}
pub(crate) use string_enum;

/// Lets a wire field that stays a `String` be built from and compared with a
/// `string_enum!`'s values: `event.kind == EventKind::OperationFailed`.
macro_rules! compared_as_string {
    ($name:ident) => {
        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.as_str().to_owned()
            }
        }

        impl PartialEq<$name> for String {
            fn eq(&self, value: &$name) -> bool {
                self == value.as_str()
            }
        }

        impl PartialEq<$name> for str {
            fn eq(&self, value: &$name) -> bool {
                self == value.as_str()
            }
        }
    };
}
pub(crate) use compared_as_string;

use std::ops::RangeInclusive;

/// Byte lengths of one DNS label.
pub const DNS_LABEL: RangeInclusive<usize> = 1..=63;

/// Whether `value` has `lengths` bytes of ASCII letters, digits, or hyphens,
/// and starts and ends with a letter or digit; with [`DNS_LABEL`] lengths, it
/// is one DNS label.
///
/// ```text
/// "web", "Api-2"     -> label
/// "-web", "web-", "" -> not a label
/// ```
#[must_use]
pub fn is_label(value: &str, lengths: RangeInclusive<usize>) -> bool {
    lengths.contains(&value.len())
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Whether `value` can be a request ID, the `Idempotency-Key` of a
/// mutation: 1 to 128 ASCII letters, digits, `-`, `_`, `.` or `:`.
#[must_use]
pub fn is_request_id(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
}

/// [`is_label`] without uppercase letters: the form of piqueld's IDs, names,
/// and hostnames.
pub(crate) fn is_lowercase_label(value: &str, lengths: RangeInclusive<usize>) -> bool {
    is_label(value, lengths) && !value.bytes().any(|byte| byte.is_ascii_uppercase())
}

logical_name!(
    /// User-facing logical application name.
    ApplicationName,
    ApplicationNameError,
    "application names"
);
logical_name!(
    /// User-facing environment name, unique within its application.
    EnvironmentName,
    EnvironmentNameError,
    "environment names"
);
validated_string!(
    /// A valid Git branch name, such as the branch a preview deploys.
    GitBranch,
    GitBranchError,
    "branch must be a valid Git branch name",
    crate::manifest::valid_git_branch
);
logical_name!(
    /// Distinguishes several previews of one branch, e.g. one per agent.
    PreviewSlot,
    PreviewSlotError,
    "preview slots"
);
validated_string!(
    /// DNS-safe identifier of a preview, derived once by
    /// [`PreviewSlug::derive`] and persisted. It is also the preview's name,
    /// so no environment of its application can share it.
    PreviewSlug,
    PreviewSlugError,
    "preview slugs must be 1-40 lowercase letters, digits, or hyphens, start with a letter, and end with a letter or digit",
    |value: &str| value.len() <= PreviewSlug::MAX_LEN && crate::resource::valid_logical_name(value)
);

/// A slug is a valid name, and previews are named by their slug.
impl From<PreviewSlug> for EnvironmentName {
    fn from(slug: PreviewSlug) -> Self {
        Self(slug.0)
    }
}

logical_name!(
    /// Logical service name, distinct from an application or volume name.
    ///
    /// ```compile_fail
    /// use piqueld_core::{ServiceName, VolumeName};
    /// let volume: VolumeName = ServiceName::parse("data").unwrap();
    /// ```
    ServiceName,
    ServiceNameError,
    "service names"
);
logical_name!(
    /// Logical volume name, including references from service mounts.
    VolumeName,
    VolumeNameError,
    "volume names"
);
logical_name!(
    /// Optional stable name of a route, unique within its application. Every
    /// environment and preview renders the same routes, so a name selects the
    /// same route in each, whatever hostname it renders.
    RouteName,
    RouteNameError,
    "route names"
);
logical_name!(
    /// Logical one-shot job name.
    JobName,
    JobNameError,
    "job names"
);
logical_name!(
    /// Unique name of a notification destination, such as `ops`.
    DestinationName,
    DestinationNameError,
    "destination names"
);

#[cfg(test)]
mod tests {
    use super::{ApplicationName, ServiceName, VolumeName};

    #[test]
    fn logical_names_enforce_boundaries_during_parsing_and_decoding() {
        for value in ["a".into(), "a-1".into(), "a".repeat(63)] {
            let wire = serde_json::to_string(&value).expect("the value serializes");
            assert_eq!(
                serde_json::from_str::<ApplicationName>(&wire)
                    .expect("the application name deserializes")
                    .as_str(),
                value
            );
            assert_eq!(
                serde_json::from_str::<ServiceName>(&wire)
                    .expect("the service name deserializes")
                    .as_str(),
                value
            );
            assert_eq!(
                serde_json::from_str::<VolumeName>(&wire)
                    .expect("the volume name deserializes")
                    .as_str(),
                value
            );
        }
        for value in [
            String::new(),
            "1a".into(),
            "a-".into(),
            "A".into(),
            "é".into(),
            "a".repeat(64),
        ] {
            let wire = serde_json::to_string(&value).expect("the value serializes");
            ApplicationName::parse(&value)
                .expect_err("`ApplicationName::parse` should be rejected");
            ServiceName::parse(&value).expect_err("`ServiceName::parse` should be rejected");
            VolumeName::parse(&value).expect_err("`VolumeName::parse` should be rejected");
            serde_json::from_str::<ApplicationName>(&wire)
                .expect_err("`serde_json::from_str` should be rejected");
            serde_json::from_str::<ServiceName>(&wire)
                .expect_err("`serde_json::from_str` should be rejected");
            serde_json::from_str::<VolumeName>(&wire)
                .expect_err("`serde_json::from_str` should be rejected");
        }
    }

    #[test]
    fn names_debug_as_their_quoted_value() {
        let name = ApplicationName::parse("notes").expect("the application name is valid");
        assert_eq!(format!("application {name:?}"), r#"application "notes""#);
    }
}

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdError {
    Prefix,
    NotCanonical,
    Version,
}

impl std::fmt::Display for IdError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Prefix => "id prefix does not match",
            Self::NotCanonical => "id is not a canonical prefixed uuid",
            Self::Version => "id is not uuid version 7",
        })
    }
}

impl std::error::Error for IdError {}

macro_rules! prefixed_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub const PREFIX: &'static str = $prefix;

            #[must_use]
            pub fn generate() -> Self {
                Self(format!(
                    "{}_{}",
                    Self::PREFIX,
                    Uuid::now_v7().as_hyphenated()
                ))
            }

            pub fn parse(value: impl AsRef<str>) -> Result<Self, IdError> {
                parse_prefixed(Self::PREFIX, value.as_ref()).map(Self)
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::parse(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

prefixed_id!(SessionId, "sess");
prefixed_id!(RunId, "run");
prefixed_id!(AttemptId, "attempt");
prefixed_id!(EventId, "evt");
prefixed_id!(ArtifactId, "artifact");
prefixed_id!(FeedbackId, "fbk");

fn parse_prefixed(prefix: &str, value: &str) -> Result<String, IdError> {
    let Some(rest) = value.strip_prefix(prefix) else {
        return Err(IdError::Prefix);
    };
    let Some(rest) = rest.strip_prefix('_') else {
        return Err(IdError::Prefix);
    };
    if rest.chars().any(|character| character.is_ascii_uppercase()) {
        return Err(IdError::NotCanonical);
    }
    let uuid = Uuid::parse_str(rest).map_err(|_| IdError::NotCanonical)?;
    if uuid.get_version_num() != 7 {
        return Err(IdError::Version);
    }
    let canonical = format!("{prefix}_{}", uuid.as_hyphenated());
    if canonical != value {
        return Err(IdError::NotCanonical);
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::{AttemptId, IdError, RunId, SessionId};

    #[test]
    fn generated_session_id_parses() {
        let session_id = SessionId::generate();
        assert_eq!(SessionId::parse(session_id.as_str()).unwrap(), session_id);
    }

    #[test]
    fn rejects_uuid_v4() {
        let error = RunId::parse("run_550e8400-e29b-41d4-a716-446655440000").unwrap_err();
        assert_eq!(error, IdError::Version);
    }

    #[test]
    fn rejects_wrong_prefix() {
        let session_id = SessionId::generate();
        let error = AttemptId::parse(session_id.as_str()).unwrap_err();
        assert_eq!(error, IdError::Prefix);
    }
}

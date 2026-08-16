use std::fmt;
use std::str::FromStr;

use thiserror::Error;

use crate::{Architecture, Version};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Reference {
    pub channel: String,
    pub id: String,
    pub version: Version,
    pub architecture: Architecture,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FuzzyReference {
    pub channel: Option<String>,
    pub id: String,
    pub version: Option<String>,
    pub architecture: Option<Architecture>,
}

#[derive(Debug, Error)]
pub enum ReferenceError {
    #[error("invalid reference: {0}")]
    Invalid(String),
    #[error(transparent)]
    Version(#[from] crate::version::VersionError),
    #[error(transparent)]
    Architecture(#[from] crate::architecture::ArchitectureError),
}

impl Reference {
    pub fn new(
        channel: impl Into<String>,
        id: impl Into<String>,
        version: Version,
        architecture: Architecture,
    ) -> Result<Self, ReferenceError> {
        let channel = channel.into();
        let id = id.into();
        if channel.is_empty() || id.is_empty() {
            return Err(ReferenceError::Invalid(format!("{channel}:{id}")));
        }
        Ok(Self {
            channel,
            id,
            version,
            architecture,
        })
    }

    pub fn semantic_match(&self, fuzzy: &FuzzyReference) -> bool {
        self.id == fuzzy.id
            && fuzzy
                .channel
                .as_ref()
                .is_none_or(|channel| channel == &self.channel)
            && fuzzy
                .architecture
                .is_none_or(|architecture| architecture == self.architecture)
            && fuzzy
                .version
                .as_ref()
                .is_none_or(|version| self.version.semantic_match(version))
    }
}

impl FromStr for Reference {
    type Err = ReferenceError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let (channel, remainder) = raw
            .split_once(':')
            .ok_or_else(|| ReferenceError::Invalid(raw.to_string()))?;
        let parts = remainder.split('/').collect::<Vec<_>>();
        if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
            return Err(ReferenceError::Invalid(raw.to_string()));
        }
        Self::new(
            channel,
            parts[0],
            Version::parse(parts[1])?,
            parts[2].parse()?,
        )
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}:{}/{}/{}",
            self.channel, self.id, self.version, self.architecture
        )
    }
}

impl FuzzyReference {
    pub fn new(
        channel: Option<String>,
        id: impl Into<String>,
        version: Option<String>,
        architecture: Option<Architecture>,
    ) -> Result<Self, ReferenceError> {
        let id = id.into();
        if id.is_empty() || channel.as_ref().is_some_and(String::is_empty) {
            return Err(ReferenceError::Invalid(id));
        }
        Ok(Self {
            channel,
            id,
            version,
            architecture,
        })
    }
}

impl FromStr for FuzzyReference {
    type Err = ReferenceError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let (channel, remainder) =
            raw.split_once(':')
                .map_or((None, raw), |(channel, remainder)| {
                    (
                        (channel != "unknown" && !channel.is_empty()).then(|| channel.to_string()),
                        remainder,
                    )
                });
        let parts = remainder.split('/').collect::<Vec<_>>();
        if parts.is_empty() || parts.len() > 3 || parts[0].is_empty() {
            return Err(ReferenceError::Invalid(raw.to_string()));
        }
        let version = parts
            .get(1)
            .filter(|value| !value.is_empty() && **value != "unknown")
            .map(|value| (*value).to_string());
        let architecture = parts
            .get(2)
            .filter(|value| !value.is_empty() && **value != "unknown")
            .map(|value| Architecture::from_str(value))
            .transpose()?;
        Self::new(channel, parts[0], version, architecture)
    }
}

impl fmt::Display for FuzzyReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}:{}/{}/{}",
            self.channel.as_deref().unwrap_or("unknown"),
            self.id,
            self.version.as_deref().unwrap_or("unknown"),
            self.architecture
                .map(|architecture| architecture.to_string())
                .as_deref()
                .unwrap_or("unknown")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_references_round_trip() {
        for raw in [
            "main:com.example.App/1.0.0.0/x86_64",
            "some_channel:com.example.App/1.0.0.0/x86_64",
            "main:com.example.App/1.0.0.1/arm64",
            "main:1111/1.0.0.0/x86_64",
        ] {
            assert_eq!(raw.parse::<Reference>().unwrap().to_string(), raw);
        }
    }

    #[test]
    fn rejects_original_invalid_references() {
        for raw in [
            "main:com.example.App//1.0.0.0/x86_64",
            "main:1111/1.0.0.0/ x86_64",
            "main:2222/1.0.0.0/unknown",
            ":1.0.0.1-beta/arm64",
            ":com.example.App/1.0.0.0/x86_64",
        ] {
            assert!(raw.parse::<Reference>().is_err(), "{raw}");
        }
    }

    #[test]
    fn fuzzy_references_expand_unknown_fields() {
        let cases = [
            (
                "unknown:com.example.App/1.0.0.0/x86_64",
                "unknown:com.example.App/1.0.0.0/x86_64",
            ),
            (
                "com.example.App/1.0.0.0/x86_64",
                "unknown:com.example.App/1.0.0.0/x86_64",
            ),
            (
                "com.example.App/unknown/x86_64",
                "unknown:com.example.App/unknown/x86_64",
            ),
            (
                "com.example.App/1.0.0.0/unknown",
                "unknown:com.example.App/1.0.0.0/unknown",
            ),
            (
                "com.example.App/1.0.0.0",
                "unknown:com.example.App/1.0.0.0/unknown",
            ),
            ("com.example.App", "unknown:com.example.App/unknown/unknown"),
            (
                "com.example.App/1.0.0.1",
                "unknown:com.example.App/1.0.0.1/unknown",
            ),
            ("3333/1.0.0.0/arm64", "unknown:3333/1.0.0.0/arm64"),
            ("4444/1.0.0.1/arm64", "unknown:4444/1.0.0.1/arm64"),
        ];
        for (raw, expected) in cases {
            assert_eq!(raw.parse::<FuzzyReference>().unwrap().to_string(), expected);
        }
    }

    #[test]
    fn exact_reference_semantically_matches_fuzzy_version() {
        let reference = "main:org.deepin.base/23.0.0.1/x86_64"
            .parse::<Reference>()
            .unwrap();
        for (raw, expected) in [
            ("main:org.deepin.base/23.0.0/x86_64", true),
            ("org.deepin.base/23.0.0/x86_64", true),
            ("main:org.example.base/23.0.0/x86_64", false),
            ("stable:org.deepin.base/23.0.0/x86_64", false),
            ("main:org.deepin.base/23.0.0/arm64", false),
            ("main:org.deepin.base/24.0.0/x86_64", false),
        ] {
            assert_eq!(
                reference.semantic_match(&raw.parse().unwrap()),
                expected,
                "{raw}"
            );
        }
    }
}

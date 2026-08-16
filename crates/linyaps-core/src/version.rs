use std::cmp::Ordering;
use std::fmt;

use semver::{BuildMetadata, Prerelease};
use thiserror::Error;

#[derive(Clone, Copy, Debug)]
pub struct ParseOptions {
    pub strict: bool,
    pub fallback: bool,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            strict: true,
            fallback: true,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Version {
    V2(VersionV2),
    V1(VersionV1),
    Fallback(FallbackVersion),
}

#[derive(Debug, Error)]
pub enum VersionError {
    #[error("invalid version: {0}")]
    Invalid(String),
    #[error("numeric version component is too large: {0}")]
    Overflow(String),
}

impl Version {
    pub fn parse(raw: &str) -> Result<Self, VersionError> {
        Self::parse_with_options(raw, ParseOptions::default())
    }

    pub fn parse_with_options(raw: &str, options: ParseOptions) -> Result<Self, VersionError> {
        if let Ok(version) = VersionV2::parse(raw, options.strict) {
            return Ok(Self::V2(version));
        }
        if !options.fallback {
            return Err(VersionError::Invalid(raw.to_string()));
        }
        if let Ok(version) = VersionV1::parse(raw) {
            return Ok(Self::V1(version));
        }
        FallbackVersion::parse(raw).map(Self::Fallback)
    }

    pub fn semantic_match(&self, raw: &str) -> bool {
        match self {
            Self::V2(version) => version.semantic_match(raw),
            Self::V1(version) => version.semantic_match(raw),
            Self::Fallback(version) => version.semantic_match(raw),
        }
    }

    pub fn validate_depend_version(raw: &str) -> Result<(), VersionError> {
        let parts = raw.split('.').collect::<Vec<_>>();
        if !(parts.len() == 2 || parts.len() == 3)
            || parts.iter().any(|part| {
                part.is_empty()
                    || (part.len() > 1 && part.starts_with('0'))
                    || !part.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            return Err(VersionError::Invalid(raw.to_string()));
        }
        Ok(())
    }

    pub fn ignore_tweak(&mut self) {
        if let Self::V1(version) = self {
            version.tweak = None;
        }
    }

    pub fn is_v1(&self) -> bool {
        matches!(self, Self::V1(_))
    }

    pub fn has_tweak(&self) -> bool {
        matches!(self, Self::V1(VersionV1 { tweak: Some(_), .. }))
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::V2(left), Self::V2(right)) => left == right,
            (Self::V1(left), Self::V1(right)) => left == right,
            (Self::Fallback(left), Self::Fallback(right)) => left == right,
            (Self::V1(left), Self::V2(right)) | (Self::V2(right), Self::V1(left)) => {
                compare_v1_v2(left, right) == Ordering::Equal
            }
            (Self::Fallback(left), right) | (right, Self::Fallback(left)) => {
                left.compare_raw(&right.to_string()) == Ordering::Equal
            }
        }
    }
}

impl Eq for Version {}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::V2(version) => version.fmt(formatter),
            Self::V1(version) => version.fmt(formatter),
            Self::Fallback(version) => version.fmt(formatter),
        }
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(match (self, other) {
            (Self::V2(left), Self::V2(right)) => left.cmp(right),
            (Self::V1(left), Self::V1(right)) => left.compare(right)?,
            (Self::Fallback(left), Self::Fallback(right)) => left.cmp(right),
            (Self::V1(left), Self::V2(right)) => compare_v1_v2(left, right),
            (Self::V2(left), Self::V1(right)) => compare_v1_v2(right, left).reverse(),
            (Self::Fallback(left), right) => left.compare_raw(&right.to_string()),
            (left, Self::Fallback(right)) => right.compare_raw(&left.to_string()).reverse(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VersionV1 {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub tweak: Option<u64>,
}

impl VersionV1 {
    pub fn parse(raw: &str) -> Result<Self, VersionError> {
        let parts = raw.split('.').collect::<Vec<_>>();
        if !(parts.len() == 3 || parts.len() == 4) {
            return Err(VersionError::Invalid(raw.to_string()));
        }
        let values = parts
            .iter()
            .map(|part| parse_numeric(part))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            major: values[0],
            minor: values[1],
            patch: values[2],
            tweak: values.get(3).copied(),
        })
    }

    pub fn semantic_match(&self, raw: &str) -> bool {
        let Ok(fuzzy) = Self::parse(raw) else {
            return false;
        };
        self.major == fuzzy.major
            && self.minor == fuzzy.minor
            && self.patch == fuzzy.patch
            && match (self.tweak, fuzzy.tweak) {
                (Some(left), Some(right)) => left == right,
                _ => true,
            }
    }

    fn numeric_tuple(&self) -> (u64, u64, u64, u64) {
        (self.major, self.minor, self.patch, self.tweak.unwrap_or(0))
    }

    fn compare(&self, other: &Self) -> Option<Ordering> {
        if self == other {
            return Some(Ordering::Equal);
        }
        let ordering = self.numeric_tuple().cmp(&other.numeric_tuple());
        (ordering != Ordering::Equal).then_some(ordering)
    }
}

impl fmt::Display for VersionV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(tweak) = self.tweak {
            write!(formatter, ".{tweak}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct VersionV2 {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub prerelease: Prerelease,
    pub build_metadata: BuildMetadata,
    pub security: u64,
    pub has_patch: bool,
}

impl VersionV2 {
    pub fn parse(raw: &str, strict: bool) -> Result<Self, VersionError> {
        let candidate = if strict {
            raw.to_string()
        } else {
            let raw = raw.strip_prefix('v').unwrap_or(raw);
            let numeric_end = raw.find(['-', '+']).unwrap_or(raw.len());
            let numeric = &raw[..numeric_end];
            if numeric.split('.').count() == 2 {
                format!("{numeric}.0{}", &raw[numeric_end..])
            } else {
                raw.to_string()
            }
        };
        let parsed = semver::Version::parse(&candidate)
            .map_err(|_| VersionError::Invalid(raw.to_string()))?;
        validate_prerelease_numeric(parsed.pre.as_str())?;
        let numeric_end = raw.find(['-', '+']).unwrap_or(raw.len());
        let numeric = raw[..numeric_end]
            .strip_prefix('v')
            .unwrap_or(&raw[..numeric_end]);
        let has_patch = numeric.split('.').count() == 3;
        if strict && !has_patch {
            return Err(VersionError::Invalid(raw.to_string()));
        }
        let security = extract_security(parsed.build.as_str())?;
        Ok(Self {
            major: parsed.major,
            minor: parsed.minor,
            patch: parsed.patch,
            prerelease: parsed.pre,
            build_metadata: parsed.build,
            security,
            has_patch,
        })
    }

    pub fn semantic_match(&self, raw: &str) -> bool {
        let Ok(fuzzy) = Self::parse(raw, false) else {
            return false;
        };
        self.major == fuzzy.major
            && self.minor == fuzzy.minor
            && (!fuzzy.has_patch || self.patch == fuzzy.patch)
    }
}

impl PartialEq for VersionV2 {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for VersionV2 {}

impl Ord for VersionV2 {
    fn cmp(&self, other: &Self) -> Ordering {
        (
            self.major,
            self.minor,
            self.patch,
            &self.prerelease,
            self.security,
        )
            .cmp(&(
                other.major,
                other.minor,
                other.patch,
                &other.prerelease,
                other.security,
            ))
    }
}

impl PartialOrd for VersionV2 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for VersionV2 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.prerelease.is_empty() {
            write!(formatter, "-{}", self.prerelease)?;
        }
        if !self.build_metadata.is_empty() {
            write!(formatter, "+{}", self.build_metadata)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FallbackVersion {
    pub parts: Vec<String>,
}

impl FallbackVersion {
    pub fn parse(raw: &str) -> Result<Self, VersionError> {
        let parts = raw
            .split('.')
            .filter(|part| !part.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        if parts.is_empty() {
            return Err(VersionError::Invalid(raw.to_string()));
        }
        Ok(Self { parts })
    }

    pub fn semantic_match(&self, raw: &str) -> bool {
        let parts = raw
            .split('.')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>();
        !parts.is_empty()
            && parts.len() <= self.parts.len()
            && parts
                .iter()
                .zip(&self.parts)
                .all(|(left, right)| *left == right)
    }

    fn compare_raw(&self, raw: &str) -> Ordering {
        Self::parse(raw).map_or(Ordering::Equal, |other| self.cmp(&other))
    }
}

impl Ord for FallbackVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        for (left, right) in self.parts.iter().zip(&other.parts) {
            let ordering = match (left.parse::<i32>(), right.parse::<i32>()) {
                (Ok(left), Ok(right)) => left.cmp(&right),
                _ => left.cmp(right),
            };
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        self.parts.len().cmp(&other.parts.len())
    }
}

impl PartialOrd for FallbackVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for FallbackVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.parts.join("."))
    }
}

fn parse_numeric(part: &str) -> Result<u64, VersionError> {
    if part.is_empty()
        || (part.len() > 1 && part.starts_with('0'))
        || !part.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(VersionError::Invalid(part.to_string()));
    }
    let value = part
        .parse::<u64>()
        .map_err(|_| VersionError::Overflow(part.to_string()))?;
    if value > i64::MAX as u64 {
        return Err(VersionError::Overflow(part.to_string()));
    }
    Ok(value)
}

fn validate_prerelease_numeric(prerelease: &str) -> Result<(), VersionError> {
    for part in prerelease.split('.') {
        if !part.is_empty()
            && part.bytes().all(|byte| byte.is_ascii_digit())
            && part.parse::<u64>().is_err()
        {
            return Err(VersionError::Overflow(part.to_string()));
        }
    }
    Ok(())
}

fn extract_security(build: &str) -> Result<u64, VersionError> {
    let Some(position) = build.find("security.") else {
        return Ok(0);
    };
    let value = &build[position + "security.".len()..];
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(VersionError::Invalid(build.to_string()));
    }
    let value = value
        .parse::<u64>()
        .map_err(|_| VersionError::Overflow(value.to_string()))?;
    if value == 0 {
        return Err(VersionError::Invalid(build.to_string()));
    }
    Ok(value)
}

fn compare_v1_v2(v1: &VersionV1, v2: &VersionV2) -> Ordering {
    let base = (v1.major, v1.minor, v1.patch).cmp(&(v2.major, v2.minor, v2.patch));
    if base != Ordering::Equal {
        return base;
    }
    if v1.tweak.is_none() && v2.prerelease.is_empty() && v2.security == 0 {
        return Ordering::Equal;
    }
    if v1.tweak.is_none() && v2.prerelease.is_empty() && v2.security != 0 {
        return Ordering::Less;
    }
    Ordering::Greater
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v2_security_extension() {
        let version = VersionV2::parse("1.2.3-alpha+build123.security.4", true).unwrap();
        assert_eq!(version.prerelease.as_str(), "alpha");
        assert_eq!(version.build_metadata.as_str(), "build123.security.4");
        assert_eq!(version.security, 4);
        assert_eq!(version.to_string(), "1.2.3-alpha+build123.security.4");
    }

    #[test]
    fn fuzzy_v2_may_omit_patch() {
        let version = VersionV2::parse("23.1.4", true).unwrap();
        assert!(version.semantic_match("23.1"));
        assert!(version.semantic_match("v23.1"));
        assert!(!version.semantic_match("23.2"));
    }

    #[test]
    fn v1_tweak_is_optional_for_semantic_match() {
        let version = VersionV1::parse("23.0.0.1").unwrap();
        assert!(version.semantic_match("23.0.0"));
        assert!(version.semantic_match("23.0.0.1"));
        assert!(!version.semantic_match("23.0.0.2"));
    }

    #[test]
    fn fallback_accepts_upstream_inputs() {
        for raw in [
            "1.2.a",
            "1.2.3_alpha",
            "1.2.3@#$%^&*()",
            "1.2.3 alpha",
            "1.2.3-测试",
            "1.2.3-😊",
            "1.2.3\n4\t5",
        ] {
            assert_eq!(FallbackVersion::parse(raw).unwrap().to_string(), raw);
        }
        assert!(FallbackVersion::parse("").is_err());
        assert!(FallbackVersion::parse("...").is_err());
    }

    #[test]
    fn chooses_same_parser_order_as_upstream() {
        assert!(matches!(Version::parse("1.2.3").unwrap(), Version::V2(_)));
        assert!(matches!(Version::parse("1.2.3.0").unwrap(), Version::V1(_)));
        assert!(matches!(
            Version::parse("1.2.a").unwrap(),
            Version::Fallback(_)
        ));
    }

    #[test]
    fn compares_security_after_prerelease() {
        let base = VersionV2::parse("1.2.3", true).unwrap();
        let security = VersionV2::parse("1.2.3+security.2", true).unwrap();
        assert!(security > base);
        let prerelease = VersionV2::parse("1.2.3-beta+security.9", true).unwrap();
        assert!(prerelease < base);
    }

    #[test]
    fn accepts_and_round_trips_upstream_versions() {
        for raw in [
            "0.0.0.4",
            "1.2.3.4",
            "10.20.30.40",
            "999999999999999999.999999999999999999.99999999999999999.99999999999999999",
            "1.0.0",
            "1.0.0-alpha",
            "1.0.0+build.1",
            "1.0.0-alpha+build.1",
            "1.0.0+buildinfo.security.1",
            "1.0.0-alpha+buildinfo.security.1",
            "1.0.0+security.2",
        ] {
            assert_eq!(Version::parse(raw).unwrap().to_string(), raw, "{raw}");
        }
    }

    #[test]
    fn rejects_upstream_invalid_semver_cases() {
        for raw in [
            "-1.0.0",
            "1.-1.0",
            "0.0.-1",
            "1",
            "",
            "1.0",
            "1.0-alpha",
            "1.0-alpha.01",
            "1.0-alpha.1+security.01",
            "1.0-alpha.1+security.0",
            "a1.0.0",
            "1.a0.0",
            "1.0.a0",
            "v1.0.0",
            "1.2.3-0123",
            "1.2.3-0123.0123",
            "1.1.2+.123",
            "1.0.0-alpha_beta",
            "9.8.7+meta+meta",
            "1.2.3.DEV",
        ] {
            assert!(VersionV2::parse(raw, true).is_err(), "{raw}");
        }
    }

    #[test]
    fn loose_semver_matches_upstream_normalization() {
        for (raw, expected, has_patch) in [
            ("v1.2.3", "1.2.3", true),
            ("1.2", "1.2.0", false),
            ("v1.2", "1.2.0", false),
            ("v1.2.3-alpha+build", "1.2.3-alpha+build", true),
            ("1.2-alpha+build", "1.2.0-alpha+build", false),
            ("v1.2-alpha+build", "1.2.0-alpha+build", false),
        ] {
            let version = VersionV2::parse(raw, false).unwrap();
            assert_eq!(version.to_string(), expected, "{raw}");
            assert_eq!(version.has_patch, has_patch, "{raw}");
        }
    }

    #[test]
    fn v2_equality_ignores_build_metadata_but_not_security() {
        let plain = VersionV2::parse("5.2.3-alpha.2", true).unwrap();
        let build_34 = VersionV2::parse("5.2.3-alpha.2+build.34", true).unwrap();
        let build_35 = VersionV2::parse("5.2.3-alpha.2+build.35", true).unwrap();
        assert_eq!(plain, build_34);
        assert_eq!(build_34, build_35);

        let security_1 = VersionV2::parse("5.2.3-alpha.2+security.1", true).unwrap();
        let security_2 = VersionV2::parse("5.2.3-alpha.2+security.2", true).unwrap();
        assert_ne!(plain, security_1);
        assert!(security_1 < security_2);

        let loose = VersionV2::parse("5.2", false).unwrap();
        let strict = VersionV2::parse("5.2.0", true).unwrap();
        assert_eq!(loose, strict);
    }

    #[test]
    fn version_equality_dispatches_across_representations() {
        assert_eq!(
            Version::V1(VersionV1::parse("1.2.3").unwrap()),
            Version::V2(VersionV2::parse("1.2.3+build.9", true).unwrap())
        );
        assert_ne!(
            Version::V1(VersionV1::parse("1.2.3.0").unwrap()),
            Version::V2(VersionV2::parse("1.2.3", true).unwrap())
        );
        assert_eq!(
            Version::Fallback(FallbackVersion::parse("1.2.3").unwrap()),
            Version::V2(VersionV2::parse("1.2.3", true).unwrap())
        );
        assert_ne!(
            Version::Fallback(FallbackVersion::parse("1.2.3").unwrap()),
            Version::V2(VersionV2::parse("1.2.3+build.9", true).unwrap())
        );
    }

    #[test]
    fn follows_upstream_version_ordering_sequence() {
        let versions = [
            "1.0.0-alpha",
            "1.0.0-beta",
            "1.0.0-rc",
            "1.0.0+buildinfo.security.1",
            "1.0.0+security.2",
            "1.0.0.0",
            "1.0.0.1",
            "2.0.0.0",
            "2.1.0.0",
            "2.1.1.0",
            "2.1.1.1",
            "3.1.6",
        ]
        .map(|raw| Version::parse(raw).unwrap());
        for (index, left) in versions.iter().enumerate() {
            for right in &versions[index + 1..] {
                assert!(left < right, "{left} should be less than {right}");
            }
        }
    }

    #[test]
    fn compares_prereleases_by_semver_rules() {
        let versions = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
        ]
        .map(|raw| VersionV2::parse(raw, true).unwrap());
        for pair in versions.windows(2) {
            assert!(
                pair[0] < pair[1],
                "{} should be less than {}",
                pair[0],
                pair[1]
            );
        }
        assert!(
            VersionV2::parse("5.2.3-alpha.2", true).unwrap()
                < VersionV2::parse("5.2.3-alpha.a", true).unwrap()
        );
    }

    #[test]
    fn v1_uses_upstream_signed_parse_limit() {
        assert!(VersionV1::parse("9223372036854775807.0.0").is_ok());
        assert!(VersionV1::parse("9223372036854775808.0.0").is_err());
        assert!(matches!(
            Version::parse("9223372036854775808.0.0.0").unwrap(),
            Version::Fallback(_)
        ));
    }

    #[test]
    fn rejects_prerelease_numeric_overflow_like_upstream() {
        assert!(VersionV2::parse("1.2.3-18446744073709551615", true).is_ok());
        assert!(VersionV2::parse("1.2.3-18446744073709551616", true).is_err());
    }

    #[test]
    fn validates_dependency_version_shape() {
        for raw in ["0.0", "1.2", "1.2.3", "999999999999999999999.2.3"] {
            assert!(Version::validate_depend_version(raw).is_ok(), "{raw}");
        }
        for raw in ["1", "1.2.3.4", "01.2", "1.02", "1.a", "1.2-"] {
            assert!(Version::validate_depend_version(raw).is_err(), "{raw}");
        }
    }
}

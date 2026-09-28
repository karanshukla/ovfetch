//! Deciding whether a hash can be trusted before anything is downloaded.

use crate::data::Ledger;
use crate::sources::Claim;
use anyhow::{Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    /// Every reachable source agrees and so does the reviewed ledger.
    Verified,
    /// Every reachable source agrees, but the ledger has never seen it.
    Unverified,
}

/// Minimum sources that must answer, so one reachable host cannot vouch for itself.
pub const QUORUM: usize = 2;

/// The agreed digest, or an error naming every source when any disagree.
pub fn agree(id: &str, claims: &[Claim], ledger: &Ledger) -> Result<(String, Trust)> {
    let answered: Vec<&Claim> = claims.iter().filter(|c| c.digest.is_some()).collect();
    for c in claims.iter().filter(|c| c.error.is_some()) {
        eprintln!(
            "note: {} did not answer for {id}: {}",
            c.source,
            c.error.as_deref().unwrap_or_default()
        );
    }
    if answered.len() < QUORUM {
        bail!(
            "only {} source(s) answered for {id}, need {QUORUM}; refusing to trust a single host",
            answered.len()
        );
    }
    let first = answered[0].digest.clone().unwrap_or_default();
    if answered
        .iter()
        .any(|c| c.digest.as_deref() != Some(first.as_str()))
    {
        let table: Vec<String> = answered
            .iter()
            .map(|c| format!("  {}  {}", c.digest.as_deref().unwrap_or("-"), c.source))
            .collect();
        bail!(
            "SOURCES DISAGREE on the sha256 of {id}. Nothing was installed.\n{}\n\
             This is what a tampered mirror or package looks like. Report it before retrying.",
            table.join("\n")
        );
    }
    match ledger.get(id) {
        Some(e) if e.digest == first => Ok((first, Trust::Verified)),
        Some(e) => bail!(
            "{id} is recorded in the ledger as {} (first seen {}), but every source now says {first}.\n\
             A published artifact changed after release. Nothing was installed.",
            e.digest,
            e.first_seen
        ),
        None => Ok((first, Trust::Unverified)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Entry;

    fn claim(source: &str, digest: Option<&str>) -> Claim {
        Claim {
            source: source.into(),
            digest: digest.map(str::to_owned),
            error: digest.is_none().then(|| "down".into()),
        }
    }

    fn ledger(digest: &str) -> Ledger {
        Ledger {
            list: vec![Entry {
                id: "x".into(),
                digest: digest.into(),
                first_seen: "2026-09-27".into(),
                provenance: false,
            }],
        }
    }

    #[test]
    fn agreeing_sources_matching_the_ledger_are_verified() {
        let claims = [
            claim("a", Some("aa")),
            claim("b", Some("aa")),
            claim("c", None),
        ];
        assert_eq!(
            agree("x", &claims, &ledger("aa")).unwrap(),
            ("aa".into(), Trust::Verified)
        );
    }

    #[test]
    fn unknown_to_the_ledger_is_unverified() {
        let claims = [claim("a", Some("aa")), claim("b", Some("aa"))];
        assert_eq!(
            agree("x", &claims, &Ledger::default()).unwrap().1,
            Trust::Unverified
        );
    }

    #[test]
    fn one_disagreeing_mirror_aborts() {
        let claims = [
            claim("a", Some("aa")),
            claim("b", Some("aa")),
            claim("c", Some("bb")),
        ];
        assert!(
            agree("x", &claims, &Ledger::default())
                .unwrap_err()
                .to_string()
                .contains("DISAGREE")
        );
    }

    #[test]
    fn a_changed_hash_aborts_even_when_sources_agree() {
        let claims = [claim("a", Some("bb")), claim("b", Some("bb"))];
        assert!(
            agree("x", &claims, &ledger("aa"))
                .unwrap_err()
                .to_string()
                .contains("changed after release")
        );
    }

    #[test]
    fn a_single_reachable_source_is_not_enough() {
        let claims = [claim("a", Some("aa")), claim("b", None)];
        assert!(agree("x", &claims, &Ledger::default()).is_err());
    }
}

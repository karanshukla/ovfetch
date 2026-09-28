use std::cmp::Ordering;
use std::fmt;

/// Dotted numeric version. Non-numeric suffixes (`.dev20260925`, `b1`) are
/// dropped, so a nightly compares equal to its release line.
#[derive(Clone, Debug)]
pub struct Ver(pub Vec<u64>);

impl Ver {
    pub fn parse(s: &str) -> Option<Ver> {
        let parts: Vec<u64> = s
            .trim_start_matches('v')
            .split('.')
            .map_while(|p| {
                let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
                digits.parse().ok()
            })
            .collect();
        (!parts.is_empty()).then_some(Ver(parts))
    }

    /// `2026.4.0` -> `2026.4`, the granularity device floors are expressed in.
    pub fn minor(&self) -> Ver {
        Ver(self.0.iter().take(2).copied().collect())
    }
}

impl Ord for Ver {
    fn cmp(&self, other: &Self) -> Ordering {
        let n = self.0.len().max(other.0.len());
        (0..n)
            .map(|i| {
                let a = self.0.get(i).copied().unwrap_or(0);
                let b = other.0.get(i).copied().unwrap_or(0);
                a.cmp(&b)
            })
            .find(|o| o.is_ne())
            .unwrap_or(Ordering::Equal)
    }
}

impl PartialEq for Ver {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Ver {}

impl PartialOrd for Ver {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Ver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s: Vec<String> = self.0.iter().map(u64::to_string).collect();
        f.write_str(&s.join("."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_numerically_and_pads() {
        assert!(Ver::parse("2026.10").unwrap() > Ver::parse("2026.9.9").unwrap());
        assert_eq!(
            Ver::parse("2026.2").unwrap(),
            Ver::parse("2026.2.0").unwrap()
        );
    }

    #[test]
    fn strips_prerelease_suffixes() {
        assert_eq!(
            Ver::parse("2026.5.0.dev20260925").unwrap(),
            Ver::parse("2026.5.0").unwrap()
        );
        assert_eq!(Ver::parse("v1.38.0").unwrap().to_string(), "1.38.0");
    }
}

//! Small cookie header accumulator for TikTok's server session cookies.

#[derive(Default)]
pub(crate) struct CookieJar {
    values: Vec<(String, String)>,
}

impl CookieJar {
    pub(crate) fn absorb(&mut self, set_cookie: &str) {
        let pair = set_cookie.split(';').next().unwrap_or_default();
        self.set(pair);
    }

    pub(crate) fn set(&mut self, pair: &str) {
        let Some((name, value)) = pair.split_once('=') else {
            return;
        };
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() || value.is_empty() {
            return;
        }
        if let Some(existing) = self
            .values
            .iter_mut()
            .find(|(existing, _)| existing == name)
        {
            existing.1 = value.to_owned();
        } else {
            self.values.push((name.to_owned(), value.to_owned()));
        }
    }

    pub(crate) fn header(&self) -> Option<String> {
        if self.values.is_empty() {
            return None;
        }
        Some(
            self.values
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_replaces() {
        let mut jar = CookieJar::default();
        jar.absorb("ttwid=abc; Path=/; HttpOnly");
        jar.absorb("waf_cookie=xyz; Path=/");
        assert_eq!(jar.header().as_deref(), Some("ttwid=abc; waf_cookie=xyz"));
        jar.set("waf_cookie=second");
        assert_eq!(
            jar.header().as_deref(),
            Some("ttwid=abc; waf_cookie=second")
        );
    }

    #[test]
    fn ignores_invalid_pairs() {
        let mut jar = CookieJar::default();
        jar.absorb("novalue; Path=/");
        jar.absorb("=empty");
        assert!(jar.header().is_none());
    }
}

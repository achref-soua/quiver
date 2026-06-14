// SPDX-License-Identifier: AGPL-3.0-only
//! Role-based access control over scoped API keys (ADR-0011).
//!
//! Each API key carries a **role** — the highest [`Action`] it may perform,
//! which implies the lesser ones (`Admin` ⊇ `Write` ⊇ `Read`) — and a
//! [`CollectionScope`] restricting which collections it can touch. Access is
//! **default-deny**: every operation is checked against the caller's
//! [`Principal`] at the engine-facing op layer (`AppState`), so neither
//! transport can forget to enforce it and a key can only ever reach the
//! collections in its scope.
//!
//! Keys are provisioned through configuration. A bare secret string (the
//! `QUIVER_API_KEYS` env form, or a plain TOML array entry) is an
//! all-collections **admin** key, preserving the pre-RBAC behaviour; a
//! structured entry pins a narrower role and collection scope:
//!
//! ```toml
//! # quiver.toml — an admin key plus a least-privilege, namespace-scoped key
//! api_keys = ["full-admin-secret"]
//!
//! [[api_keys]]
//! secret = "readonly-acme-secret"
//! role = "read"
//! collections = ["acme.*"]   # exact names, or a trailing-`*` prefix; "*" = all
//! ```
//!
//! A trailing-`*` pattern matches by prefix, which gives namespacing: a key
//! scoped to `acme.*` reaches `acme.orders` but not `beta.orders`. (Avoid `/`
//! in collection names — the REST API addresses a collection as one path
//! segment.)

use serde::de::{SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

use crate::Error;

/// An action a caller may be permitted to perform, ordered by privilege so that
/// a higher role implies the lower ones (`Read < Write < Admin`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Read access: get, search, list, and inspect collections.
    Read,
    /// Write access (implies read): upsert and delete points.
    Write,
    /// Administrative access (implies write): create and delete collections.
    Admin,
}

/// Which collections a key may touch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CollectionScope {
    /// Every collection.
    #[default]
    All,
    /// Only collections matching one of these patterns. A pattern ending in `*`
    /// matches by prefix (e.g. `acme.*` matches `acme.orders`); otherwise it is
    /// an exact name.
    Patterns(Vec<String>),
}

impl CollectionScope {
    /// Build a scope from configured patterns: any `*` widens to [`All`].
    ///
    /// [`All`]: CollectionScope::All
    fn from_patterns(patterns: Vec<String>) -> Self {
        if patterns.is_empty() || patterns.iter().any(|p| p == "*") {
            CollectionScope::All
        } else {
            CollectionScope::Patterns(patterns)
        }
    }

    /// Whether `collection` is within this scope.
    #[must_use]
    pub fn matches(&self, collection: &str) -> bool {
        match self {
            CollectionScope::All => true,
            CollectionScope::Patterns(patterns) => {
                patterns.iter().any(|p| pattern_matches(p, collection))
            }
        }
    }
}

// `acme.*` matches any name starting with `acme.`; `*` matches everything; any
// other pattern is an exact match.
fn pattern_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

/// A configured API key: a bearer secret, the role it grants, and the
/// collections it is scoped to.
#[derive(Debug, Clone, Serialize)]
pub struct ApiKey {
    /// The bearer secret presented as `Authorization: Bearer <secret>`.
    pub secret: String,
    /// The highest action this key may perform.
    pub role: Action,
    /// The collections this key may touch.
    pub collections: CollectionScope,
}

impl ApiKey {
    /// An all-collections admin key — the meaning of a bare secret string.
    #[must_use]
    pub fn admin(secret: impl Into<String>) -> Self {
        Self {
            secret: secret.into(),
            role: Action::Admin,
            collections: CollectionScope::All,
        }
    }
}

impl From<&str> for ApiKey {
    fn from(secret: &str) -> Self {
        ApiKey::admin(secret)
    }
}

impl From<String> for ApiKey {
    fn from(secret: String) -> Self {
        ApiKey::admin(secret)
    }
}

/// The authenticated caller's effective authority for one request.
#[derive(Debug, Clone)]
pub(crate) struct Principal {
    role: Action,
    collections: CollectionScope,
}

impl Principal {
    /// The implicit principal in `insecure` mode (no keys configured): full
    /// access, matching the pre-auth behaviour.
    pub(crate) fn insecure() -> Self {
        Self {
            role: Action::Admin,
            collections: CollectionScope::All,
        }
    }

    fn from_key(key: &ApiKey) -> Self {
        Self {
            role: key.role,
            collections: key.collections.clone(),
        }
    }

    /// Authorize `action` on an optional `collection`, returning
    /// [`Error::Forbidden`] when the role is too low or the collection is out of
    /// scope. With `collection = None` (collection-agnostic listing) only the
    /// role is checked; results are then narrowed with [`Principal::can_see`].
    pub(crate) fn require(&self, action: Action, collection: Option<&str>) -> Result<(), Error> {
        let role_ok = self.role >= action;
        let scope_ok = collection.is_none_or(|c| self.collections.matches(c));
        if role_ok && scope_ok {
            Ok(())
        } else {
            Err(Error::Forbidden(
                "the API key's scope does not permit this operation".to_owned(),
            ))
        }
    }

    /// Whether this principal may see `collection` (used to filter list results
    /// so a key never learns the names of collections outside its scope).
    pub(crate) fn can_see(&self, collection: &str) -> bool {
        self.collections.matches(collection)
    }
}

/// Match a presented bearer secret against the configured keys in constant time,
/// returning the caller's [`Principal`]. With no keys configured (`insecure`
/// mode, enforced at startup) any caller is the [`Principal::insecure`] admin.
/// `None` means authentication failed (a 401).
pub(crate) fn authenticate(keys: &[ApiKey], presented: Option<&str>) -> Option<Principal> {
    if keys.is_empty() {
        return Some(Principal::insecure());
    }
    let token = presented?;
    let mut matched: Option<&ApiKey> = None;
    // Check every key (no early exit) so timing does not reveal which matched.
    for key in keys {
        if constant_time_eq(key.secret.as_bytes(), token.as_bytes()) {
            matched = Some(key);
        }
    }
    matched.map(Principal::from_key)
}

// Length-checked constant-time byte comparison for API keys.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

// Deserialize `CollectionScope` from a sequence of string patterns.
impl<'de> Deserialize<'de> for CollectionScope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let patterns = Vec::<String>::deserialize(deserializer)?;
        Ok(CollectionScope::from_patterns(patterns))
    }
}

impl Serialize for CollectionScope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            CollectionScope::All => ["*"].serialize(serializer),
            CollectionScope::Patterns(patterns) => patterns.serialize(serializer),
        }
    }
}

// A structured key entry as written in TOML (`role` required, `collections`
// optional and defaulting to all).
#[derive(Deserialize)]
struct KeySpec {
    secret: String,
    role: Action,
    #[serde(default)]
    collections: CollectionScope,
}

// Accept the API key list as a comma-separated string (the `QUIVER_API_KEYS`
// env form, which figment surfaces as a scalar), or a sequence whose entries are
// either a bare secret (an admin key) or a structured `{secret, role,
// collections}` table.
pub(crate) fn de_api_keys<'de, D>(deserializer: D) -> Result<Vec<ApiKey>, D::Error>
where
    D: Deserializer<'de>,
{
    struct KeysVisitor;

    impl<'de> Visitor<'de> for KeysVisitor {
        type Value = Vec<ApiKey>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a comma-separated string of secrets, or a list of secrets/key tables")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ApiKey::admin)
                .collect())
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            #[derive(Deserialize)]
            #[serde(untagged)]
            enum Entry {
                Plain(String),
                Structured(KeySpec),
            }
            let mut keys = Vec::new();
            while let Some(entry) = seq.next_element::<Entry>()? {
                keys.push(match entry {
                    Entry::Plain(secret) => ApiKey::admin(secret),
                    Entry::Structured(spec) => ApiKey {
                        secret: spec.secret,
                        role: spec.role,
                        collections: spec.collections,
                    },
                });
            }
            Ok(keys)
        }
    }

    deserializer.deserialize_any(KeysVisitor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_ordering_implies_lesser_privileges() {
        assert!(Action::Admin > Action::Write);
        assert!(Action::Write > Action::Read);
    }

    #[test]
    fn collection_scope_matches_exact_prefix_and_all() {
        let all = CollectionScope::from_patterns(vec!["*".to_owned()]);
        assert!(matches!(all, CollectionScope::All));
        assert!(all.matches("anything"));

        let scoped = CollectionScope::from_patterns(vec!["acme.*".to_owned(), "shared".to_owned()]);
        assert!(scoped.matches("acme.orders"));
        assert!(scoped.matches("shared"));
        assert!(!scoped.matches("beta.orders"));
        assert!(!scoped.matches("acme")); // the `acme.` prefix (with the dot) is required
        assert!(!scoped.matches("shared2"));
    }

    #[test]
    fn require_enforces_role_and_scope() {
        let reader = Principal {
            role: Action::Read,
            collections: CollectionScope::Patterns(vec!["acme.*".to_owned()]),
        };
        assert!(reader.require(Action::Read, Some("acme.orders")).is_ok());
        // Over-scope on action: a reader cannot write.
        assert!(reader.require(Action::Write, Some("acme.orders")).is_err());
        // Over-scope on collection: a reader cannot read another namespace.
        assert!(reader.require(Action::Read, Some("beta.orders")).is_err());
        // Collection-agnostic listing checks the role only.
        assert!(reader.require(Action::Read, None).is_ok());
        assert!(reader.can_see("acme.orders"));
        assert!(!reader.can_see("beta.orders"));
    }

    #[test]
    fn insecure_principal_is_admin_over_all() {
        let p = Principal::insecure();
        assert!(p.require(Action::Admin, Some("anything")).is_ok());
        assert!(p.can_see("anything"));
    }

    #[test]
    fn authenticate_matches_secret_and_denies_others() {
        let keys = vec![
            ApiKey::admin("admin-secret"),
            ApiKey {
                secret: "reader-secret".to_owned(),
                role: Action::Read,
                collections: CollectionScope::Patterns(vec!["acme.*".to_owned()]),
            },
        ];
        // No keys ⇒ insecure ⇒ any caller is admin.
        assert!(authenticate(&[], None).is_some());
        // A valid secret resolves to its principal.
        let reader = authenticate(&keys, Some("reader-secret")).expect("reader authenticates");
        assert!(reader.require(Action::Write, Some("acme.x")).is_err());
        // A wrong or missing secret is denied.
        assert!(authenticate(&keys, Some("nope")).is_none());
        assert!(authenticate(&keys, None).is_none());
    }

    #[test]
    fn de_api_keys_parses_csv_strings_and_structured_tables() {
        #[derive(Deserialize)]
        struct Wrap {
            #[serde(deserialize_with = "de_api_keys")]
            api_keys: Vec<ApiKey>,
        }

        // Comma-separated string (the env form) ⇒ trimmed admin keys.
        let csv: Wrap = serde_json::from_str(r#"{"api_keys":"a, b ,c"}"#).unwrap();
        assert_eq!(csv.api_keys.len(), 3);
        assert!(csv.api_keys.iter().all(|k| k.role == Action::Admin));
        assert_eq!(csv.api_keys[1].secret, "b");

        // A sequence mixing a bare secret and a structured, scoped key.
        let mixed: Wrap = serde_json::from_str(
            r#"{"api_keys":["root",{"secret":"ro","role":"read","collections":["acme.*"]}]}"#,
        )
        .unwrap();
        assert_eq!(mixed.api_keys[0].role, Action::Admin);
        assert!(matches!(
            mixed.api_keys[0].collections,
            CollectionScope::All
        ));
        assert_eq!(mixed.api_keys[1].role, Action::Read);
        assert!(mixed.api_keys[1].collections.matches("acme.x"));
        assert!(!mixed.api_keys[1].collections.matches("beta.x"));

        // A structured key without `collections` defaults to all.
        let defaulted: Wrap =
            serde_json::from_str(r#"{"api_keys":[{"secret":"w","role":"write"}]}"#).unwrap();
        assert!(matches!(
            defaulted.api_keys[0].collections,
            CollectionScope::All
        ));
    }
}

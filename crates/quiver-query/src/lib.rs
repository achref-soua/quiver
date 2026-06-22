// SPDX-License-Identifier: AGPL-3.0-only
//! Query-side filtering: a typed predicate tree over a point's JSON payload.
//!
//! How a filter runs is chosen by the embeddable database's planner: when the
//! filter is selective on secondary-indexed fields it pre-filters to an exact
//! candidate scan, and otherwise it post-filters the vector-search candidates
//! (see `quiver-embed` and `docs/index/design.md`). Either way the [`Filter`]
//! tree is the stable wire shape and is re-checked on every surviving candidate,
//! so results are exact.
//!
//! Field references are dot-paths into the payload object (`"user.age"`).

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod sparse;
pub mod sparse_index;
pub use sparse::{DEFAULT_RRF_K0, SPARSE_KEY, SparseVector, rrf_fuse};
pub use sparse_index::SparseInvertedIndex;

/// A predicate over a point's JSON payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Filter {
    /// All sub-filters must match.
    And(Vec<Filter>),
    /// At least one sub-filter must match.
    Or(Vec<Filter>),
    /// The sub-filter must not match.
    Not(Box<Filter>),
    /// Field equals value (numbers compared numerically across int/float).
    Eq {
        /// Dot-path to the field.
        field: String,
        /// Value to compare against.
        value: Value,
    },
    /// Field is absent or not equal to value.
    Ne {
        /// Dot-path to the field.
        field: String,
        /// Value to compare against.
        value: Value,
    },
    /// Field equals one of the values.
    In {
        /// Dot-path to the field.
        field: String,
        /// Allowed values.
        values: Vec<Value>,
    },
    /// Field is strictly less than value (numbers or strings).
    Lt {
        /// Dot-path to the field.
        field: String,
        /// Value to compare against.
        value: Value,
    },
    /// Field is less than or equal to value.
    Lte {
        /// Dot-path to the field.
        field: String,
        /// Value to compare against.
        value: Value,
    },
    /// Field is strictly greater than value.
    Gt {
        /// Dot-path to the field.
        field: String,
        /// Value to compare against.
        value: Value,
    },
    /// Field is greater than or equal to value.
    Gte {
        /// Dot-path to the field.
        field: String,
        /// Value to compare against.
        value: Value,
    },
    /// Field is present (any value, including null).
    Exists {
        /// Dot-path to the field.
        field: String,
    },
}

impl Filter {
    /// Evaluate the predicate against a payload. A missing field makes
    /// value-comparisons (`Eq`/`In`/`Lt`/…) false; `Ne` is true for a missing
    /// field; `Exists` reports presence.
    #[must_use]
    pub fn matches(&self, payload: &Value) -> bool {
        match self {
            Filter::And(subs) => subs.iter().all(|f| f.matches(payload)),
            Filter::Or(subs) => subs.iter().any(|f| f.matches(payload)),
            Filter::Not(sub) => !sub.matches(payload),
            Filter::Eq { field, value } => {
                field_value(payload, field).is_some_and(|v| values_eq(v, value))
            }
            Filter::Ne { field, value } => {
                !field_value(payload, field).is_some_and(|v| values_eq(v, value))
            }
            Filter::In { field, values } => field_value(payload, field)
                .is_some_and(|v| values.iter().any(|candidate| values_eq(v, candidate))),
            Filter::Lt { field, value } => compares(payload, field, value, |o| o == Ordering::Less),
            Filter::Lte { field, value } => {
                compares(payload, field, value, |o| o != Ordering::Greater)
            }
            Filter::Gt { field, value } => {
                compares(payload, field, value, |o| o == Ordering::Greater)
            }
            Filter::Gte { field, value } => {
                compares(payload, field, value, |o| o != Ordering::Less)
            }
            Filter::Exists { field } => field_value(payload, field).is_some(),
        }
    }
}

// Resolve a dot-path into a payload object.
fn field_value<'a>(payload: &'a Value, field: &str) -> Option<&'a Value> {
    let mut current = payload;
    for part in field.split('.') {
        current = current.get(part)?;
    }
    Some(current)
}

// Equality with numeric coercion: 1 and 1.0 compare equal.
fn values_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        },
        _ => a == b,
    }
}

// Order two values when comparable (number/number or string/string).
fn order(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64()?.partial_cmp(&y.as_f64()?),
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

fn compares(payload: &Value, field: &str, value: &Value, pred: impl Fn(Ordering) -> bool) -> bool {
    field_value(payload, field)
        .and_then(|v| order(v, value))
        .is_some_and(pred)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn p() -> Value {
        json!({"city": "paris", "age": 30, "score": 4.5, "tags": ["a", "b"], "user": {"vip": true}})
    }

    #[test]
    fn eq_and_ne() {
        assert!(
            Filter::Eq {
                field: "city".into(),
                value: json!("paris")
            }
            .matches(&p())
        );
        assert!(
            !Filter::Eq {
                field: "city".into(),
                value: json!("lyon")
            }
            .matches(&p())
        );
        // numeric coercion: 30 == 30.0
        assert!(
            Filter::Eq {
                field: "age".into(),
                value: json!(30.0)
            }
            .matches(&p())
        );
        assert!(
            Filter::Ne {
                field: "city".into(),
                value: json!("lyon")
            }
            .matches(&p())
        );
        // Ne on a missing field is true.
        assert!(
            Filter::Ne {
                field: "missing".into(),
                value: json!(1)
            }
            .matches(&p())
        );
    }

    #[test]
    fn ranges_and_in_and_exists() {
        assert!(
            Filter::Gt {
                field: "age".into(),
                value: json!(18)
            }
            .matches(&p())
        );
        assert!(
            Filter::Lte {
                field: "score".into(),
                value: json!(4.5)
            }
            .matches(&p())
        );
        assert!(
            !Filter::Lt {
                field: "age".into(),
                value: json!(30)
            }
            .matches(&p())
        );
        assert!(
            Filter::In {
                field: "city".into(),
                values: vec![json!("paris"), json!("lyon")]
            }
            .matches(&p())
        );
        assert!(
            Filter::Exists {
                field: "user.vip".into()
            }
            .matches(&p())
        );
        assert!(
            !Filter::Exists {
                field: "user.nope".into()
            }
            .matches(&p())
        );
        // a comparison against a missing field is false
        assert!(
            !Filter::Gt {
                field: "missing".into(),
                value: json!(0)
            }
            .matches(&p())
        );
    }

    #[test]
    fn boolean_composition_and_nested_paths() {
        let f = Filter::And(vec![
            Filter::Eq {
                field: "city".into(),
                value: json!("paris"),
            },
            Filter::Or(vec![
                Filter::Gt {
                    field: "age".into(),
                    value: json!(100),
                },
                Filter::Eq {
                    field: "user.vip".into(),
                    value: json!(true),
                },
            ]),
            Filter::Not(Box::new(Filter::Eq {
                field: "city".into(),
                value: json!("lyon"),
            })),
        ]);
        assert!(f.matches(&p()));
    }

    #[test]
    fn filter_roundtrips_through_json() {
        let f = Filter::And(vec![
            Filter::Eq {
                field: "a".into(),
                value: json!(1),
            },
            Filter::Exists { field: "b".into() },
        ]);
        let text = serde_json::to_string(&f).unwrap();
        let back: Filter = serde_json::from_str(&text).unwrap();
        assert_eq!(f, back);
    }
}

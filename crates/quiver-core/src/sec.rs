// SPDX-License-Identifier: AGPL-3.0-only
//! Secondary indexes (`.sec`): per filterable field, value → roaring bitmap of
//! the rows that hold it, the substrate for pre-filtered (hybrid) search
//! (ADR-0022).
//!
//! At flush time, for each field declared filterable in the collection schema,
//! the engine extracts the field's value from every row's JSON payload and builds
//! a sorted `(key → rows)` map. Keys are **order-preserving** — UTF-8 for keyword
//! fields, a sign-flipped big-endian encoding for numeric fields — so an equality
//! query is a binary search and a range query is a contiguous scan. Each
//! segment's index is written to `seg-NNN.sec`; queries union the per-segment
//! results (the caller intersects with liveness via the primary index).
//!
//! [`SecValue`] / [`SecPredicate`] are the indexable predicate vocabulary; the
//! richer [`quiver_query`](https://docs.rs/quiver-query)-style filter tree is
//! decomposed into these by the query planner.

use std::cmp::Ordering;

use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::descriptor::{FieldType, FilterableField};
use crate::error::{CoreError, Result};

/// A concrete value to match against a filterable field.
#[derive(Debug, Clone, PartialEq)]
pub enum SecValue {
    /// An exact-match string, for a [`FieldType::Keyword`] field.
    Keyword(String),
    /// A number, for a [`FieldType::Numeric`] field.
    Numeric(f64),
}

/// An indexable predicate over a single filterable field, answered through the
/// secondary index.
#[derive(Debug, Clone, PartialEq)]
pub enum SecPredicate {
    /// The field equals `value`.
    Eq {
        /// Dot-path to the field.
        field: String,
        /// Value to match.
        value: SecValue,
    },
    /// The field equals one of `values`.
    In {
        /// Dot-path to the field.
        field: String,
        /// Allowed values.
        values: Vec<SecValue>,
    },
    /// The field falls within the (optionally open) bounds.
    Range {
        /// Dot-path to the field.
        field: String,
        /// Lower bound, if any.
        lo: Option<SecValue>,
        /// Upper bound, if any.
        hi: Option<SecValue>,
        /// Whether the lower bound is inclusive.
        lo_inclusive: bool,
        /// Whether the upper bound is inclusive.
        hi_inclusive: bool,
    },
}

impl SecPredicate {
    /// The field this predicate constrains.
    #[must_use]
    pub fn field(&self) -> &str {
        match self {
            SecPredicate::Eq { field, .. }
            | SecPredicate::In { field, .. }
            | SecPredicate::Range { field, .. } => field,
        }
    }
}

// Encode a finite f64 into 8 order-preserving big-endian bytes: byte-lexical
// order then matches numeric order, negatives included. (NaN has no order and is
// rejected by the caller.)
fn encode_f64(x: f64) -> [u8; 8] {
    let bits = x.to_bits();
    let ordered = if bits >> 63 == 0 {
        bits | (1 << 63)
    } else {
        !bits
    };
    ordered.to_be_bytes()
}

// Resolve a dot-path into a JSON payload object.
fn field_value<'a>(payload: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = payload;
    for part in path.split('.') {
        current = current.get(part)?;
    }
    Some(current)
}

// Encode a payload's field value into its order-preserving key, per the field's
// type. `None` if absent, the wrong JSON type, or NaN — such a row is simply not
// indexed (and does not match equality/range, exactly like a missing field).
fn encode_field_value(field_type: FieldType, value: &Value) -> Option<Vec<u8>> {
    match field_type {
        FieldType::Keyword => match value {
            Value::String(s) => Some(s.as_bytes().to_vec()),
            _ => None,
        },
        FieldType::Numeric => match value {
            Value::Number(n) => {
                let x = n.as_f64()?;
                (!x.is_nan()).then(|| encode_f64(x).to_vec())
            }
            _ => None,
        },
    }
}

// Encode a query value, checking it matches the field's declared type. A
// type-mismatched value encodes to `None` and therefore matches nothing.
fn encode_sec_value(field_type: FieldType, value: &SecValue) -> Option<Vec<u8>> {
    match (field_type, value) {
        (FieldType::Keyword, SecValue::Keyword(s)) => Some(s.as_bytes().to_vec()),
        (FieldType::Numeric, SecValue::Numeric(x)) => {
            (!x.is_nan()).then(|| encode_f64(*x).to_vec())
        }
        _ => None,
    }
}

/// One field's index: order-sorted keys with a parallel array of serialized
/// roaring bitmaps (the rows holding each key).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FieldIndex {
    path: String,
    field_type: FieldType,
    // Ascending by `keys[i]` (order-preserving); `bitmaps[i]` are the rows.
    keys: Vec<Vec<u8>>,
    bitmaps: Vec<Vec<u8>>,
}

impl FieldIndex {
    fn bitmap_at(&self, i: usize) -> Result<RoaringBitmap> {
        let bytes = self.bitmaps.get(i).ok_or_else(|| {
            CoreError::MalformedPage("secondary-index bitmap out of range".into())
        })?;
        Ok(RoaringBitmap::deserialize_from(&bytes[..])?)
    }

    fn equals(&self, value: &SecValue) -> Result<RoaringBitmap> {
        let Some(key) = encode_sec_value(self.field_type, value) else {
            return Ok(RoaringBitmap::new());
        };
        match self.keys.binary_search(&key) {
            Ok(i) => self.bitmap_at(i),
            Err(_) => Ok(RoaringBitmap::new()),
        }
    }

    fn range(
        &self,
        lo: Option<&SecValue>,
        hi: Option<&SecValue>,
        lo_inclusive: bool,
        hi_inclusive: bool,
    ) -> Result<RoaringBitmap> {
        // A provided bound that cannot encode (type mismatch) makes the predicate
        // unsatisfiable.
        let lo_key = match lo {
            Some(v) => match encode_sec_value(self.field_type, v) {
                Some(k) => Some(k),
                None => return Ok(RoaringBitmap::new()),
            },
            None => None,
        };
        let hi_key = match hi {
            Some(v) => match encode_sec_value(self.field_type, v) {
                Some(k) => Some(k),
                None => return Ok(RoaringBitmap::new()),
            },
            None => None,
        };
        let mut out = RoaringBitmap::new();
        for (i, key) in self.keys.iter().enumerate() {
            if let Some(l) = &lo_key {
                let c = key.as_slice().cmp(l.as_slice());
                if c == Ordering::Less || (c == Ordering::Equal && !lo_inclusive) {
                    continue;
                }
            }
            if let Some(h) = &hi_key {
                let c = key.as_slice().cmp(h.as_slice());
                if c == Ordering::Greater || (c == Ordering::Equal && !hi_inclusive) {
                    continue;
                }
            }
            out |= self.bitmap_at(i)?;
        }
        Ok(out)
    }
}

/// A segment's secondary index over its collection's filterable fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct SecIndex {
    fields: Vec<FieldIndex>,
}

impl SecIndex {
    /// Build the index for `payloads` (row `r` is `payloads[r]`, JSON bytes) over
    /// the declared `filterable` fields.
    pub(crate) fn build(filterable: &[FilterableField], payloads: &[&[u8]]) -> Result<Self> {
        // Per field, a sorted map from encoded key to the rows holding it.
        let mut maps: Vec<std::collections::BTreeMap<Vec<u8>, RoaringBitmap>> =
            vec![std::collections::BTreeMap::new(); filterable.len()];
        for (row, payload) in payloads.iter().enumerate() {
            let Ok(value) = serde_json::from_slice::<Value>(payload) else {
                continue; // a non-JSON payload contributes no indexed fields
            };
            for (i, field) in filterable.iter().enumerate() {
                if let Some(fv) = field_value(&value, &field.path)
                    && let Some(key) = encode_field_value(field.field_type, fv)
                {
                    maps[i].entry(key).or_default().insert(row as u32);
                }
            }
        }
        let mut fields = Vec::with_capacity(filterable.len());
        for (field, map) in filterable.iter().zip(maps) {
            let mut keys = Vec::with_capacity(map.len());
            let mut bitmaps = Vec::with_capacity(map.len());
            for (key, bitmap) in map {
                let mut buf = Vec::with_capacity(bitmap.serialized_size());
                bitmap.serialize_into(&mut buf)?;
                keys.push(key);
                bitmaps.push(buf);
            }
            fields.push(FieldIndex {
                path: field.path.clone(),
                field_type: field.field_type,
                keys,
                bitmaps,
            });
        }
        Ok(Self { fields })
    }

    /// Serialize the index to `postcard` bytes for the `.sec` file.
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        Ok(postcard::to_allocvec(self)?)
    }

    /// Decode an index from `.sec` bytes.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        Ok(postcard::from_bytes(bytes)?)
    }

    /// The rows matching `predicate`, or `None` if its field is not indexed in
    /// this segment (so the caller cannot pre-filter on it).
    pub(crate) fn query(&self, predicate: &SecPredicate) -> Result<Option<RoaringBitmap>> {
        let Some(field) = self.fields.iter().find(|f| f.path == predicate.field()) else {
            return Ok(None);
        };
        let bitmap = match predicate {
            SecPredicate::Eq { value, .. } => field.equals(value)?,
            SecPredicate::In { values, .. } => {
                let mut out = RoaringBitmap::new();
                for value in values {
                    out |= field.equals(value)?;
                }
                out
            }
            SecPredicate::Range {
                lo,
                hi,
                lo_inclusive,
                hi_inclusive,
                ..
            } => field.range(lo.as_ref(), hi.as_ref(), *lo_inclusive, *hi_inclusive)?,
        };
        Ok(Some(bitmap))
    }
}

/// Whether a single JSON `payload` satisfies `predicate` for a field of
/// `field_type` — the un-indexed (active-buffer) evaluation path, kept in exact
/// agreement with the index by reusing its key encoding.
pub(crate) fn payload_matches(
    predicate: &SecPredicate,
    field_type: FieldType,
    payload: &[u8],
) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(payload) else {
        return false;
    };
    let Some(fv) = field_value(&value, predicate.field()) else {
        return false;
    };
    let Some(key) = encode_field_value(field_type, fv) else {
        return false;
    };
    match predicate {
        SecPredicate::Eq { value, .. } => {
            encode_sec_value(field_type, value).is_some_and(|k| k == key)
        }
        SecPredicate::In { values, .. } => values
            .iter()
            .any(|v| encode_sec_value(field_type, v).is_some_and(|k| k == key)),
        SecPredicate::Range {
            lo,
            hi,
            lo_inclusive,
            hi_inclusive,
            ..
        } => {
            let lo_ok = match lo {
                Some(v) => encode_sec_value(field_type, v).is_some_and(|l| {
                    let c = key.as_slice().cmp(l.as_slice());
                    c == Ordering::Greater || (c == Ordering::Equal && *lo_inclusive)
                }),
                None => true,
            };
            let hi_ok = match hi {
                Some(v) => encode_sec_value(field_type, v).is_some_and(|h| {
                    let c = key.as_slice().cmp(h.as_slice());
                    c == Ordering::Less || (c == Ordering::Equal && *hi_inclusive)
                }),
                None => true,
            };
            lo_ok && hi_ok
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields() -> Vec<FilterableField> {
        vec![
            FilterableField::keyword("city"),
            FilterableField::numeric("age"),
        ]
    }

    fn payloads() -> Vec<Vec<u8>> {
        vec![
            json!({"city": "paris", "age": 30}).to_string().into_bytes(),
            json!({"city": "lyon", "age": 25}).to_string().into_bytes(),
            json!({"city": "paris", "age": 40}).to_string().into_bytes(),
            json!({"city": "paris"}).to_string().into_bytes(), // missing age
        ]
    }

    fn built() -> SecIndex {
        let p = payloads();
        let refs: Vec<&[u8]> = p.iter().map(Vec::as_slice).collect();
        let idx = SecIndex::build(&fields(), &refs).unwrap();
        // Round-trip through the on-disk encoding to exercise it.
        SecIndex::decode(&idx.encode().unwrap()).unwrap()
    }

    fn rows(bm: Option<RoaringBitmap>) -> Vec<u32> {
        bm.unwrap().iter().collect()
    }

    #[test]
    fn equality_on_keyword_and_numeric() {
        let idx = built();
        assert_eq!(
            rows(
                idx.query(&SecPredicate::Eq {
                    field: "city".into(),
                    value: SecValue::Keyword("paris".into()),
                })
                .unwrap()
            ),
            vec![0, 2, 3]
        );
        assert_eq!(
            rows(
                idx.query(&SecPredicate::Eq {
                    field: "age".into(),
                    value: SecValue::Numeric(25.0),
                })
                .unwrap()
            ),
            vec![1]
        );
    }

    #[test]
    fn numeric_range_is_order_preserving() {
        let idx = built();
        // 25 <= age < 40  →  rows 1 (25) and 0 (30), not 2 (40).
        assert_eq!(
            rows(
                idx.query(&SecPredicate::Range {
                    field: "age".into(),
                    lo: Some(SecValue::Numeric(25.0)),
                    hi: Some(SecValue::Numeric(40.0)),
                    lo_inclusive: true,
                    hi_inclusive: false,
                })
                .unwrap()
            ),
            vec![0, 1]
        );
    }

    #[test]
    fn in_unions_values_and_unknown_field_is_none() {
        let idx = built();
        assert_eq!(
            rows(
                idx.query(&SecPredicate::In {
                    field: "city".into(),
                    values: vec![
                        SecValue::Keyword("lyon".into()),
                        SecValue::Keyword("paris".into())
                    ],
                })
                .unwrap()
            ),
            vec![0, 1, 2, 3]
        );
        // A field that was not declared filterable yields None (not pre-filterable).
        assert!(
            idx.query(&SecPredicate::Eq {
                field: "country".into(),
                value: SecValue::Keyword("fr".into()),
            })
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn negative_numbers_order_correctly() {
        let p: Vec<Vec<u8>> = [-5.0, 0.0, -100.0, 7.0]
            .iter()
            .map(|x| json!({ "t": x }).to_string().into_bytes())
            .collect();
        let refs: Vec<&[u8]> = p.iter().map(Vec::as_slice).collect();
        let idx = SecIndex::build(&[FilterableField::numeric("t")], &refs).unwrap();
        // t < 0  →  rows holding -5 and -100 (indices 0 and 2).
        assert_eq!(
            rows(
                idx.query(&SecPredicate::Range {
                    field: "t".into(),
                    lo: None,
                    hi: Some(SecValue::Numeric(0.0)),
                    lo_inclusive: true,
                    hi_inclusive: false,
                })
                .unwrap()
            ),
            vec![0, 2]
        );
    }

    #[test]
    fn payload_matches_agrees_with_the_index() {
        let pay = json!({"city": "paris", "age": 30}).to_string().into_bytes();
        assert!(payload_matches(
            &SecPredicate::Eq {
                field: "city".into(),
                value: SecValue::Keyword("paris".into())
            },
            FieldType::Keyword,
            &pay
        ));
        assert!(payload_matches(
            &SecPredicate::Range {
                field: "age".into(),
                lo: Some(SecValue::Numeric(18.0)),
                hi: None,
                lo_inclusive: true,
                hi_inclusive: true,
            },
            FieldType::Numeric,
            &pay
        ));
        assert!(!payload_matches(
            &SecPredicate::Eq {
                field: "city".into(),
                value: SecValue::Keyword("lyon".into())
            },
            FieldType::Keyword,
            &pay
        ));
    }
}

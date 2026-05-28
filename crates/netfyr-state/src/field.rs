use crate::{Provenance, Value};
use serde::{Deserialize, Serialize};

/// A field's value paired with its provenance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FieldValue {
    pub value: Value,
    pub provenance: Provenance,
}

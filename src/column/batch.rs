//! Schemas and batches.

use super::{Column, ColumnRef};
use crate::error::{ErrorKind, Result};
use crate::types::{DataType, ScalarValue};
use std::fmt;
use std::sync::Arc;

/// A named, typed column of a relation. `qualifier` is the table name or alias
/// the column belongs to (if any), used to resolve `t.col` references.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Field {
    pub qualifier: Option<String>,
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
}

impl Field {
    pub fn new(name: impl Into<String>, data_type: DataType, nullable: bool) -> Self {
        Field {
            qualifier: None,
            name: name.into(),
            data_type,
            nullable,
        }
    }

    pub fn with_qualifier(mut self, q: Option<String>) -> Self {
        self.qualifier = q;
        self
    }

    pub fn qualified_name(&self) -> String {
        match &self.qualifier {
            Some(q) => format!("{q}.{}", self.name),
            None => self.name.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Schema {
    pub fields: Vec<Field>,
}

pub type SchemaRef = Arc<Schema>;

impl Schema {
    pub fn new(fields: Vec<Field>) -> Self {
        Schema { fields }
    }

    pub fn empty() -> Self {
        Schema { fields: vec![] }
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    pub fn field(&self, i: usize) -> &Field {
        &self.fields[i]
    }

    /// Find a column by optional qualifier and name. Errors if the reference is
    /// ambiguous; returns `Ok(None)` if not found.
    pub fn index_of(&self, qualifier: Option<&str>, name: &str) -> Result<Option<usize>> {
        let mut found: Option<usize> = None;
        for (i, f) in self.fields.iter().enumerate() {
            let q_ok = match qualifier {
                Some(q) => f.qualifier.as_deref() == Some(q),
                None => true,
            };
            if q_ok && f.name == name {
                if let Some(prev) = found {
                    // The same (qualifier, name) twice is only ambiguous if the
                    // caller could not disambiguate it.
                    if self.fields[prev].qualifier != f.qualifier || qualifier.is_none() {
                        return Err(crate::error::Error::new(
                            ErrorKind::Bind,
                            format!(
                                "column reference \"{name}\" is ambiguous (could be {} or {})",
                                self.fields[prev].qualified_name(),
                                f.qualified_name()
                            ),
                        ));
                    }
                } else {
                    found = Some(i);
                }
            }
        }
        Ok(found)
    }

    pub fn join(&self, other: &Schema) -> Schema {
        let mut fields = self.fields.clone();
        fields.extend(other.fields.iter().cloned());
        Schema { fields }
    }

    pub fn project(&self, indices: &[usize]) -> Schema {
        Schema {
            fields: indices.iter().map(|&i| self.fields[i].clone()).collect(),
        }
    }
}

impl fmt::Display for Schema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self
            .fields
            .iter()
            .map(|fl| format!("{}:{}", fl.qualified_name(), fl.data_type))
            .collect();
        write!(f, "[{}]", parts.join(", "))
    }
}

/// A horizontal slice of a relation: equal-length columns. The schema lives in
/// the operator that produced the batch, not in the batch itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    pub columns: Vec<ColumnRef>,
    /// Kept explicitly so zero-column batches (e.g. `SELECT count(*)`) still
    /// carry a row count.
    pub num_rows: usize,
}

impl Batch {
    pub fn new(columns: Vec<ColumnRef>, num_rows: usize) -> Self {
        debug_assert!(columns.iter().all(|c| c.len() == num_rows));
        Batch { columns, num_rows }
    }

    pub fn from_columns(columns: Vec<Column>) -> Self {
        let num_rows = columns.first().map_or(0, |c| c.len());
        Batch::new(columns.into_iter().map(Arc::new).collect(), num_rows)
    }

    pub fn empty() -> Self {
        Batch {
            columns: vec![],
            num_rows: 0,
        }
    }

    /// A zero-row batch with correctly typed columns for `schema`.
    pub fn new_empty(schema: &Schema) -> Self {
        Batch {
            columns: schema
                .fields
                .iter()
                .map(|f| Arc::new(Column::new_null(f.data_type, 0)))
                .collect(),
            num_rows: 0,
        }
    }

    pub fn num_columns(&self) -> usize {
        self.columns.len()
    }

    pub fn column(&self, i: usize) -> &ColumnRef {
        &self.columns[i]
    }

    pub fn project(&self, indices: &[usize]) -> Batch {
        Batch {
            columns: indices.iter().map(|&i| self.columns[i].clone()).collect(),
            num_rows: self.num_rows,
        }
    }

    pub fn take(&self, idx: &[u32]) -> Batch {
        Batch {
            columns: self.columns.iter().map(|c| Arc::new(c.take(idx))).collect(),
            num_rows: idx.len(),
        }
    }

    pub fn slice(&self, offset: usize, len: usize) -> Batch {
        Batch {
            columns: self.columns.iter().map(|c| Arc::new(c.slice(offset, len))).collect(),
            num_rows: len,
        }
    }

    /// Concatenate batches with identical layouts into a single batch.
    pub fn concat(batches: &[Batch], num_columns: usize) -> Batch {
        if batches.len() == 1 {
            return batches[0].clone();
        }
        let num_rows = batches.iter().map(|b| b.num_rows).sum();
        let columns = (0..num_columns)
            .map(|i| {
                let cols: Vec<&Column> = batches.iter().map(|b| b.columns[i].as_ref()).collect();
                Arc::new(Column::concat(&cols))
            })
            .collect();
        Batch { columns, num_rows }
    }

    pub fn row(&self, i: usize) -> Vec<ScalarValue> {
        self.columns.iter().map(|c| c.value(i)).collect()
    }
}

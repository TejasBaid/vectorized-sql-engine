//! In-memory catalog of tables.

use crate::column::{BATCH_SIZE, Batch, Schema, SchemaRef};
use crate::error::{Error, ErrorKind, Result};
use std::collections::BTreeMap;
use std::sync::Arc;

/// A fully materialized table stored as a list of column batches.
#[derive(Debug, Clone)]
pub struct MemTable {
    pub schema: SchemaRef,
    pub batches: Vec<Batch>,
    pub num_rows: usize,
}

impl MemTable {
    pub fn new(schema: Schema, batches: Vec<Batch>) -> Self {
        let num_rows = batches.iter().map(|b| b.num_rows).sum();
        MemTable {
            schema: Arc::new(schema),
            batches,
            num_rows,
        }
    }

    pub fn empty(schema: Schema) -> Self {
        MemTable::new(schema, vec![])
    }

    /// Append rows, merging small trailing batches so repeated single-row
    /// INSERTs do not produce thousands of tiny batches.
    pub fn append(&mut self, batch: Batch) {
        if batch.num_rows == 0 {
            return;
        }
        self.num_rows += batch.num_rows;
        if let Some(last) = self.batches.last_mut()
            && last.num_rows + batch.num_rows <= BATCH_SIZE
        {
            *last = Batch::concat(&[last.clone(), batch], self.schema.len());
            return;
        }
        self.batches.push(batch);
    }
}

#[derive(Debug, Default)]
pub struct Catalog {
    tables: BTreeMap<String, Arc<MemTable>>,
}

impl Catalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, name: &str, table: MemTable) {
        self.tables.insert(name.to_lowercase(), Arc::new(table));
    }

    pub fn get(&self, name: &str) -> Option<Arc<MemTable>> {
        self.tables.get(name).cloned()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tables.contains_key(name)
    }

    pub fn drop_table(&mut self, name: &str) -> Result<()> {
        self.tables
            .remove(name)
            .map(|_| ())
            .ok_or_else(|| Error::new(ErrorKind::Bind, format!("table \"{name}\" does not exist")))
    }

    pub fn table_names(&self) -> Vec<String> {
        self.tables.keys().cloned().collect()
    }

    /// Replace a table's contents (copy-on-write: running queries keep their snapshot).
    pub fn update(&mut self, name: &str, f: impl FnOnce(&mut MemTable)) -> Result<()> {
        let t = self
            .tables
            .get_mut(name)
            .ok_or_else(|| Error::new(ErrorKind::Bind, format!("table \"{name}\" does not exist")))?;
        f(Arc::make_mut(t));
        Ok(())
    }
}

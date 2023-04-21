use std::collections::HashMap;

use super::{cell::Cell, FamilyId, RowKey};

/// A finished row. A row consists of a hash of one or more cells per
/// qualifer (cell name).
#[derive(Debug, Default, Clone)]
pub struct Row {
    /// The row's key.
    // This may be any ByteArray value.
    pub row_key: RowKey,
    /// The row's collection of cells, indexed by the family ID.
    pub cells: HashMap<FamilyId, Vec<Cell>>,
}

impl Row {
    pub fn get_cells(&self, column: &str) -> Option<Vec<Cell>> {
        self.cells.get(column).cloned()
    }

    /// get only the "top" cell value. Ignore other values.
    pub fn get_cell(&mut self, column: &str) -> Option<Cell> {
        if let Some(cells) = self.cells.get(column) {
            return cells.last().cloned();
        }
        None
    }

    pub fn add_cells(&mut self, family: &str, cells: Vec<Cell>) -> Option<Vec<Cell>> {
        self.cells.insert(family.to_owned(), cells)
    }
}

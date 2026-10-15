//! Type definitions for table coordinates and cell identifiers.
//!
//! Provides newtypes for self-documenting coordinate systems:
//! - Display coordinates: Visual positions in rendered table
//! - Data coordinates: Original data positions

use std::ops::Deref;

///// Rows /////
/// Visual row position in rendered table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DisplayRow(pub usize);

impl DisplayRow {
    /// Create a new display row
    pub fn new(row: usize) -> Self {
        Self(row)
    }

    /// Get the inner row value
    pub fn get(self) -> usize {
        self.0
    }
}

impl Deref for DisplayRow {
    type Target = usize;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Original table row position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DataRow(pub usize);

impl DataRow {
    /// Create a new data row
    pub fn new(row: usize) -> Self {
        Self(row)
    }
}

impl Deref for DataRow {
    type Target = usize;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<usize> for DisplayRow {
    fn from(row: usize) -> Self {
        DisplayRow::new(row)
    }
}

impl From<usize> for DataRow {
    fn from(row: usize) -> Self {
        DataRow::new(row)
    }
}

///// Columns /////
/// Source column position, independent of the table's display order. 0-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AnyColumn(pub usize);

impl AnyColumn {
    /// Create a new column ID
    pub fn new(col: usize) -> Self {
        Self(col)
    }

    /// Get the inner column value
    pub fn get(self) -> usize {
        self.0
    }
}

impl Deref for AnyColumn {
    type Target = usize;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<usize> for AnyColumn {
    fn from(col: usize) -> Self {
        AnyColumn::new(col)
    }
}

impl From<AnyColumn> for usize {
    fn from(value: AnyColumn) -> Self {
        *value
    }
}

///// Cells /////
/// Cell with a visual row position and source column position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DisplayCellId {
    pub row: DisplayRow,
    pub col: AnyColumn,
}

impl DisplayCellId {
    /// Create a new display cell ID
    pub fn new(row: impl Into<DisplayRow>, col: impl Into<AnyColumn>) -> Self {
        Self {
            row: row.into(),
            col: col.into(),
        }
    }

    /// Returns (row, column)
    pub fn to_raw(&self) -> (usize, usize) {
        (self.row.0, self.col.0)
    }
}

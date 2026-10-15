//! The reusable tabular-data viewer component.
//!
//! `TableView` owns the [`TableDataEngine`] (client-side filter/sort + display-to-data mapping)
//! and all of the grid rendering over `ui::Table`.
//!
//! It is deliberately source-agnostic: it renders whatever [`TableLikeContent`] it is handed
//! via [`TableView::set_contents`], whether that content comes from the CSV parser or from some
//! other producer (e.g. a database result set).

use std::{
    collections::{BTreeSet, HashMap},
    time::{Duration, Instant},
};

use editor::{Editor, EditorEvent};
use gpui::{
    App, AppContext, Entity, EventEmitter, FocusHandle, Focusable, ListAlignment, Modifiers, Point,
    Subscription, Task,
};
use ui::{
    AbsoluteLength, ResizableColumnsState, SharedString, TableInteractionState,
    TableResizeBehavior, prelude::*,
};

use crate::{
    settings::{RowRenderMechanism, TableViewSettings},
    table_data_engine::{DisplayToDataMapping, TableDataEngine},
    types::{AnyColumn, DataRow, DisplayRow, TableLikeContent},
};

/// Cell coordinates identify rows in the supplied content, independent of sorting/filtering.
#[derive(Clone, Debug, PartialEq)]
pub enum TableViewEvent {
    EditCell {
        row: usize,
        column: usize,
    },
    CellEdited {
        row: usize,
        column: usize,
        text: String,
    },
    SelectionChanged,
    ColumnOrderChanged {
        columns: Vec<String>,
    },
    ContextMenu {
        row: usize,
        column: Option<usize>,
        position: Point<Pixels>,
    },
}

pub(crate) struct CellEdit {
    pub row: usize,
    pub column: usize,
    pub editor: Entity<Editor>,
    _blur_subscription: Subscription,
    _editor_subscription: Subscription,
}

#[derive(Debug, Default)]
pub struct PerformanceMetrics {
    /// Map of timing metrics with their duration and measurement time.
    pub timings: HashMap<&'static str, (Duration, Instant)>,
    /// List of display indices that were rendered in the current frame.
    pub rendered_indices: Vec<usize>,
}

impl PerformanceMetrics {
    pub fn record<F, R>(&mut self, name: &'static str, mut f: F) -> R
    where
        F: FnMut() -> R,
    {
        let start_time = Instant::now();
        let ret = f();
        let duration = start_time.elapsed();
        self.timings.insert(name, (duration, Instant::now()));
        ret
    }

    /// Displays all metrics sorted A-Z in format: `{name}: {took}ms {ago}s ago`
    pub fn display(&self) -> String {
        let mut metrics = self.timings.iter().collect::<Vec<_>>();
        metrics.sort_by_key(|&(name, _)| *name);
        metrics
            .iter()
            .map(|(name, (duration, time))| {
                let took = duration.as_secs_f32() * 1000.;
                let ago = time.elapsed().as_secs();
                format!("{name}: {took:.3}ms {ago}s ago")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Get timing for a specific metric
    pub fn get_timing(&self, name: &str) -> Option<Duration> {
        self.timings.get(name).map(|(duration, _)| *duration)
    }
}

pub struct TableView {
    pub(crate) engine: TableDataEngine,
    pub(crate) focus_handle: FocusHandle,
    pub(crate) table_interaction_state: Entity<TableInteractionState>,
    pub(crate) column_widths: Entity<ResizableColumnsState>,
    pub(crate) column_order: Vec<AnyColumn>,
    pub(crate) column_reordering_enabled: bool,
    /// Background task computing the display-to-data mapping after a filter/sort change.
    /// Stored here so that a new change cancels the previous in-flight computation.
    pub(crate) filter_sort_task: Option<Task<()>>,
    pub(crate) settings: TableViewSettings,
    /// Performance metrics for debugging and monitoring grid operations.
    pub(crate) performance_metrics: PerformanceMetrics,
    pub(crate) list_state: gpui::ListState,
    /// Cached row height, refreshed from the actual text line height on every render.
    /// Used to size not-yet-rendered rows for the scrollbar without a full `.measure_all()`
    /// pass, so it tracks the real row height instead of a hardcoded guess.
    pub(crate) row_height: Pixels,
    /// Whether the producer feeding this view is currently computing content. While set, the grid
    /// shows a loading indicator instead of the (stale or empty) table.
    pub(crate) is_loading: bool,
    pub(crate) selection_enabled: bool,
    pub(crate) editing_enabled: bool,
    pub(crate) context_menu_enabled: bool,
    pub(crate) selected_rows: BTreeSet<usize>,
    pub(crate) selected_cell: Option<(usize, usize)>,
    selection_anchor: Option<usize>,
    pub(crate) cell_edit: Option<CellEdit>,
    pub(crate) modified_cells: BTreeSet<(usize, usize)>,
    pub(crate) deleted_rows: BTreeSet<usize>,
    pub(crate) contents_generation: u64,
    mapping_pending: bool,
    pending_reveal: Option<(u64, usize)>,
}

impl TableView {
    pub fn new(window: &Window, cx: &mut Context<Self>) -> Self {
        let contents = TableLikeContent::default();
        let table_interaction_state = cx.new(|cx| {
            TableInteractionState::new(cx).with_custom_scrollbar(ui::Scrollbars::for_settings::<
                editor::EditorSettingsScrollbarProxy,
            >())
        });
        let row_height = window.pixel_snap(window.line_height());

        Self {
            engine: TableDataEngine::default(),
            focus_handle: cx.focus_handle(),
            table_interaction_state,
            column_widths: cx.new(|_cx| {
                ResizableColumnsState::new(
                    1,
                    vec![AbsoluteLength::Pixels(px(150.))],
                    vec![TableResizeBehavior::Resizable],
                )
            }),
            column_order: Vec::new(),
            column_reordering_enabled: false,
            filter_sort_task: None,
            settings: TableViewSettings::default(),
            performance_metrics: PerformanceMetrics::default(),
            list_state: gpui::ListState::new(contents.rows.len(), ListAlignment::Top, px(1.))
                .with_uniform_item_height(row_height),
            row_height,
            is_loading: false,
            selection_enabled: false,
            editing_enabled: false,
            context_menu_enabled: false,
            selected_rows: BTreeSet::new(),
            selected_cell: None,
            selection_anchor: None,
            cell_edit: None,
            modified_cells: BTreeSet::new(),
            deleted_rows: BTreeSet::new(),
            contents_generation: 0,
            mapping_pending: false,
            pending_reveal: None,
        }
    }

    /// Replace the data shown by the grid. Recomputes filter menus and column widths, kicks off the
    /// display-to-data recomputation, and clears the loading state.
    pub fn set_contents(&mut self, contents: TableLikeContent, cx: &mut Context<Self>) {
        let column_order = self.column_order();
        self.contents_generation = self.contents_generation.wrapping_add(1);
        self.pending_reveal = None;
        self.cell_edit = None;
        self.editing_enabled = false;
        self.modified_cells.clear();
        self.deleted_rows.clear();
        self.clear_selection(cx);
        self.engine.set_contents(contents);
        self.column_order = self.resolve_column_order(&column_order);
        // The old mapping may reference rows removed by this change. Clear it immediately
        // rather than leaving the list showing stale rows until the background task below
        // recomputes the mapping.
        self.list_state
            .reset_with_uniform_height(0, self.row_height);
        self.sync_column_widths(cx);
        self.is_loading = false;
        self.apply_filter_sort(cx);
    }

    /// Column names in display order. Cell events continue to use source column indices.
    pub fn column_order(&self) -> Vec<String> {
        self.column_order
            .iter()
            .filter_map(|column| {
                self.engine
                    .contents
                    .headers
                    .get(*column)?
                    .display_value()
                    .map(ToString::to_string)
            })
            .collect()
    }

    /// Apply a display order without emitting a change event. Missing columns are appended.
    pub fn set_column_order(&mut self, columns: &[String], cx: &mut Context<Self>) {
        let order = self.resolve_column_order(columns);
        if self.column_order != order {
            self.column_order = order;
            self.list_state.remeasure();
            cx.notify();
        }
    }

    pub fn set_column_reordering_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.column_reordering_enabled = enabled;
        cx.notify();
    }

    fn resolve_column_order(&self, columns: &[String]) -> Vec<AnyColumn> {
        let mut remaining: Vec<_> = (0..self.engine.contents.number_of_cols)
            .map(AnyColumn)
            .collect();
        let mut order = Vec::with_capacity(remaining.len());
        for name in columns {
            if let Some(position) = remaining.iter().position(|column| {
                self.engine
                    .contents
                    .headers
                    .get(*column)
                    .and_then(|cell| cell.display_value())
                    .is_some_and(|header| header.as_ref() == name)
            }) {
                order.push(remaining.remove(position));
            }
        }
        order.extend(remaining);
        order
    }

    pub(crate) fn move_column(
        &mut self,
        column: AnyColumn,
        target: AnyColumn,
        cx: &mut Context<Self>,
    ) {
        if !self.column_reordering_enabled || self.is_loading || column == target {
            return;
        }
        let Some(position) = self.column_order.iter().position(|entry| *entry == column) else {
            return;
        };
        let Some(destination) = self.column_order.iter().position(|entry| *entry == target) else {
            return;
        };
        self.column_order.remove(position);
        self.column_order.insert(destination, column);
        self.list_state.remeasure();
        cx.emit(TableViewEvent::ColumnOrderChanged {
            columns: self.column_order(),
        });
        cx.notify();
    }

    /// Toggle the loading indicator (shown while a producer computes new content).
    pub fn set_loading(&mut self, is_loading: bool, cx: &mut Context<Self>) {
        self.is_loading = is_loading;
        if is_loading {
            self.cell_edit = None;
        }
        cx.notify();
    }

    pub fn set_selection_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.selection_enabled = enabled;
        if !enabled {
            self.clear_selection(cx);
        }
        cx.notify();
    }

    /// Editing must be enabled again after each replacement of the contents.
    pub fn set_editing_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.editing_enabled = enabled;
        if !enabled {
            self.cell_edit = None;
        }
        cx.notify();
    }

    /// Enable owner-managed context menus and row selection. Disabled by default so
    /// ordinary previews retain right-click-to-copy behavior.
    pub fn set_context_menu_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.context_menu_enabled = enabled;
        if enabled {
            self.selection_enabled = true;
        }
        cx.notify();
    }

    /// Selected underlying row indices, in ascending order.
    pub fn selected_rows(&self) -> Vec<usize> {
        self.selected_rows.iter().copied().collect()
    }

    pub fn selected_cell(&self) -> Option<(usize, usize)> {
        self.selected_cell
    }

    /// Restore a selection after replacing a result snapshot with the same row identities.
    pub fn set_selected_rows(
        &mut self,
        rows: impl IntoIterator<Item = usize>,
        cx: &mut Context<Self>,
    ) {
        if !self.selection_enabled {
            return;
        }
        self.selected_rows = rows
            .into_iter()
            .filter(|row| *row < self.engine.contents.rows.len())
            .collect();
        self.selection_anchor = None;
        if self
            .selected_cell
            .is_some_and(|(row, _)| !self.selected_rows.contains(&row))
        {
            self.selected_cell = None;
        }
        cx.emit(TableViewEvent::SelectionChanged);
        cx.notify();
    }

    pub fn select_all_visible(&mut self, cx: &mut Context<Self>) {
        if !self.selection_enabled || self.is_loading {
            return;
        }
        self.selected_rows = self.visible_rows().into_iter().collect();
        self.selection_anchor = None;
        self.selected_cell = None;
        cx.emit(TableViewEvent::SelectionChanged);
        cx.notify();
    }

    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        let changed = !self.selected_rows.is_empty() || self.selected_cell.is_some();
        self.selected_rows.clear();
        self.selection_anchor = None;
        self.selected_cell = None;
        if changed {
            cx.emit(TableViewEvent::SelectionChanged);
            cx.notify();
        }
    }

    fn visible_rows(&self) -> Vec<usize> {
        let mapping = self.engine.d2d_mapping();
        (0..mapping.visible_row_count())
            .filter_map(|row| mapping.get_data_row(DisplayRow(row)).map(|row| row.0))
            .collect()
    }

    pub(crate) fn select_row(
        &mut self,
        row: DataRow,
        column: Option<usize>,
        modifiers: Modifiers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selection_enabled || self.is_loading {
            return;
        }
        let visible_rows = self.visible_rows();
        let Some(position) = visible_rows
            .iter()
            .position(|candidate| *candidate == row.0)
        else {
            return;
        };
        self.cell_edit = None;
        self.focus_handle.focus(window, cx);
        let additive = if cfg!(target_os = "macos") {
            modifiers.platform
        } else {
            modifiers.control
        };
        if modifiers.shift {
            let anchor = self
                .selection_anchor
                .and_then(|anchor| visible_rows.iter().position(|row| *row == anchor))
                .unwrap_or(position);
            if !additive {
                self.selected_rows.clear();
            }
            self.selected_rows.extend(
                visible_rows
                    .iter()
                    .skip(anchor.min(position))
                    .take(anchor.abs_diff(position) + 1),
            );
        } else if additive {
            if !self.selected_rows.remove(&row.0) {
                self.selected_rows.insert(row.0);
            }
            self.selection_anchor = Some(row.0);
        } else {
            self.selected_rows.clear();
            self.selected_rows.insert(row.0);
            self.selection_anchor = Some(row.0);
        }
        self.selected_cell = column
            .filter(|_| self.selected_rows.contains(&row.0))
            .map(|column| (row.0, column));
        cx.emit(TableViewEvent::SelectionChanged);
        cx.notify();
    }

    pub(crate) fn request_context_menu(
        &mut self,
        row: DataRow,
        column: Option<usize>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.context_menu_enabled
            || self.is_loading
            || !self.visible_rows().contains(&row.0)
            || column.is_some_and(|column| column >= self.engine.contents.number_of_cols)
        {
            return;
        }
        self.emit_cell_edit(cx);
        self.focus_handle.focus(window, cx);
        let selected_cell = column.map(|column| (row.0, column));
        let selection_changed =
            !self.selected_rows.contains(&row.0) || self.selected_cell != selected_cell;
        if !self.selected_rows.contains(&row.0) {
            self.selected_rows.clear();
            self.selected_rows.insert(row.0);
            self.selection_anchor = Some(row.0);
        }
        self.selected_cell = selected_cell;
        if selection_changed {
            cx.emit(TableViewEvent::SelectionChanged);
        }
        cx.emit(TableViewEvent::ContextMenu {
            row: row.0,
            column,
            position,
        });
        cx.notify();
    }

    /// Open a native editor in an existing cell. The owner supplies the unformatted value.
    /// Enter, an outside click or losing focus emits `CellEdited`; Escape cancels.
    pub fn begin_cell_edit(
        &mut self,
        row: usize,
        column: usize,
        initial_text: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.editing_enabled
            || self.is_loading
            || self.deleted_rows.contains(&row)
            || self
                .engine
                .contents
                .rows
                .get(row)
                .and_then(|row| row.get(column))
                .is_none()
            || !self
                .engine
                .d2d_mapping()
                .retained_rows
                .contains(&DataRow(row))
        {
            return false;
        }
        self.cell_edit = None;
        let initial_text = initial_text.into();
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(initial_text, window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
            editor
        });
        let focus_handle = editor.focus_handle(cx);
        let editor_id = editor.entity_id();
        let editor_subscription = cx.subscribe(&editor, |_, _, event, cx| {
            if matches!(
                event,
                EditorEvent::Edited { .. } | EditorEvent::BufferEdited
            ) {
                cx.notify();
            }
        });
        let blur_subscription = cx.on_focus_out(&focus_handle, window, move |this, _, _, cx| {
            if this
                .cell_edit
                .as_ref()
                .is_some_and(|edit| edit.editor.entity_id() == editor_id)
            {
                this.emit_cell_edit(cx);
            }
        });
        self.cell_edit = Some(CellEdit {
            row,
            column,
            editor,
            _blur_subscription: blur_subscription,
            _editor_subscription: editor_subscription,
        });
        focus_handle.focus(window, cx);
        cx.notify();
        true
    }

    pub fn commit_cell_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.emit_cell_edit(cx) {
            self.focus_handle.focus(window, cx);
        }
    }

    pub fn is_cell_editing(&self) -> bool {
        self.cell_edit.is_some()
    }

    pub fn pending_cell_edit(&self, cx: &App) -> Option<(usize, usize, String)> {
        let edit = self.cell_edit.as_ref()?;
        Some((edit.row, edit.column, edit.editor.read(cx).text(cx)))
    }

    /// Take the current value without emitting an event, for a synchronous owner save operation.
    pub fn take_cell_edit(&mut self, cx: &mut Context<Self>) -> Option<(usize, usize, String)> {
        let edit = self.cell_edit.take()?;
        let text = edit.editor.read(cx).text(cx);
        cx.notify();
        Some((edit.row, edit.column, text))
    }

    pub(crate) fn emit_cell_edit(&mut self, cx: &mut Context<Self>) -> bool {
        let Some((row, column, text)) = self.take_cell_edit(cx) else {
            return false;
        };
        cx.emit(TableViewEvent::CellEdited { row, column, text });
        true
    }

    pub fn cancel_cell_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.cell_edit.take().is_some() {
            self.focus_handle.focus(window, cx);
            cx.notify();
        }
    }

    pub fn set_modified_cells(
        &mut self,
        cells: impl IntoIterator<Item = (usize, usize)>,
        cx: &mut Context<Self>,
    ) {
        self.modified_cells = cells
            .into_iter()
            .filter(|(row, column)| {
                self.engine
                    .contents
                    .rows
                    .get(*row)
                    .and_then(|row| row.get(*column))
                    .is_some()
            })
            .collect();
        cx.notify();
    }

    pub fn set_deleted_rows(
        &mut self,
        rows: impl IntoIterator<Item = usize>,
        cx: &mut Context<Self>,
    ) {
        self.deleted_rows = rows
            .into_iter()
            .filter(|row| *row < self.engine.contents.rows.len())
            .collect();
        if self
            .cell_edit
            .as_ref()
            .is_some_and(|edit| self.deleted_rows.contains(&edit.row))
        {
            self.cell_edit = None;
        }
        cx.notify();
    }

    pub(crate) fn sync_column_widths(&self, cx: &mut Context<Self>) {
        // plus 1 for the row identifier column
        let cols = self.engine.contents.headers.cols() + 1;
        let line_number_width = self.calculate_row_identifier_column_width();

        let mut widths: Vec<AbsoluteLength> = vec![AbsoluteLength::Pixels(px(150.)); cols];
        widths[0] = AbsoluteLength::Pixels(px(line_number_width));

        let mut resize_behaviors = vec![TableResizeBehavior::Resizable; cols];
        resize_behaviors[0] = TableResizeBehavior::None;

        self.column_widths.update(cx, |state, _cx| {
            if state.cols() != cols {
                *state = ResizableColumnsState::new(cols, widths, resize_behaviors);
            } else {
                state.set_column_configuration(
                    0,
                    AbsoluteLength::Pixels(px(line_number_width)),
                    TableResizeBehavior::None,
                );
            }
        });
    }

    pub fn clear_filters(&mut self, col: AnyColumn, cx: &mut Context<Self>) {
        self.engine.clear_filters_for_col(col);
        self.apply_filter_sort(cx);
    }

    /// Clear local column filters, leaving server-side query filters unchanged.
    pub fn clear_all_filters(&mut self, cx: &mut Context<Self>) {
        self.engine.filter_stack = Default::default();
        self.apply_filter_sort(cx);
    }

    /// Reveal a data row after any pending sort/filter mapping has been installed.
    /// A row excluded by local filters cannot be revealed without clearing those filters.
    pub fn reveal_row(&mut self, row: usize, cx: &mut Context<Self>) {
        if row >= self.engine.contents.rows.len() {
            return;
        }
        self.pending_reveal = Some((self.contents_generation, row));
        if !self.mapping_pending {
            self.reveal_pending_row(cx);
        }
    }

    fn reveal_pending_row(&mut self, cx: &mut Context<Self>) {
        let Some((generation, row)) = self.pending_reveal.take() else {
            return;
        };
        if generation != self.contents_generation {
            return;
        }
        let Some(display_row) = self
            .engine
            .d2d_mapping()
            .mapping
            .iter()
            .find_map(|(display, data)| (data.0 == row).then_some(display.0))
        else {
            return;
        };
        match self.settings.rendering_with {
            RowRenderMechanism::VariableList => self.list_state.scroll_to_reveal_item(display_row),
            RowRenderMechanism::UniformList => self
                .table_interaction_state
                .read(cx)
                .scroll_handle
                .scroll_to_item(display_row, gpui::ScrollStrategy::Nearest),
        }
        cx.notify();
    }

    pub fn toggle_filter(
        &mut self,
        col: AnyColumn,
        value: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        if let Err(err) = self.engine.toggle_filter(col, value) {
            log::error!("Failed to toggle filter: {err}");
            return;
        }
        self.apply_filter_sort(cx);
    }

    /// Spawns a background task to recompute the display-to-data mapping after a filter or sort
    /// change. Storing the task cancels any previous in-flight computation automatically.
    pub(crate) fn apply_filter_sort(&mut self, cx: &mut Context<Self>) {
        self.cell_edit = None;
        self.mapping_pending = true;
        let contents = self.engine.contents.clone();
        let filter_stack = self.engine.filter_stack.clone();
        let sorting = self.engine.applied_sorting;

        self.filter_sort_task = Some(cx.spawn(async move |this, cx| {
            let mapping = cx
                .background_spawn(async move {
                    DisplayToDataMapping::compute(&contents, &filter_stack, sorting)
                })
                .await;

            this.update(cx, |view, cx| {
                view.engine.set_d2d_mapping(mapping);
                view.mapping_pending = false;
                let visible = &view.engine.d2d_mapping().retained_rows;
                let previous_count = view.selected_rows.len();
                view.selected_rows
                    .retain(|row| visible.contains(&DataRow(*row)));
                if view
                    .selected_cell
                    .is_some_and(|(row, _)| !view.selected_rows.contains(&row))
                {
                    view.selected_cell = None;
                }
                if view.selected_rows.len() != previous_count {
                    cx.emit(TableViewEvent::SelectionChanged);
                }
                let visible_rows = view.engine.d2d_mapping().visible_row_count();
                // Uses the row height measured on the last render. Cheaper than a full
                // `.measure_all()` pass; exact row heights are re-measured on scrolling.
                view.list_state
                    .reset_with_uniform_height(visible_rows, view.row_height);
                view.reveal_pending_row(cx);
                cx.notify();
            })
            .ok();
        }));
    }
}

impl Focusable for TableView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<TableViewEvent> for TableView {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        table_data_engine::sorting_by_column::{AppliedSorting, SortDirection},
        types::TableCell,
    };
    use gpui::TestAppContext;
    use std::{cell::RefCell, rc::Rc};
    use ui::table_row::TableRow;
    use workspace::AppState;

    fn contents(values: &[&str]) -> TableLikeContent {
        TableLikeContent {
            number_of_cols: 1,
            headers: TableRow::from_vec(vec![TableCell::Generated("name".into())], 1),
            rows: values
                .iter()
                .map(|value| {
                    TableRow::from_vec(vec![TableCell::Generated((*value).to_owned().into())], 1)
                })
                .collect(),
            line_numbers: Vec::new(),
        }
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
        });
    }

    #[gpui::test]
    async fn reordered_columns_keep_source_coordinates_for_copy_selection_and_edit(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let (table, cx) = cx.add_window_view(|window, cx| TableView::new(window, cx));
        cx.update(|window, _| window.activate_window());
        let contents = TableLikeContent {
            number_of_cols: 3,
            headers: TableRow::from_vec(
                ["id", "name", "age"]
                    .map(|value| TableCell::Generated(value.into()))
                    .to_vec(),
                3,
            ),
            rows: [["1", "Grace", "37"], ["2", "Ada", "28"]]
                .into_iter()
                .map(|row| {
                    TableRow::from_vec(
                        row.map(|value| TableCell::Generated(value.into())).to_vec(),
                        3,
                    )
                })
                .collect(),
            line_numbers: Vec::new(),
        };
        let events = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update({
            let events = events.clone();
            |_, cx| {
                cx.subscribe(&table, move |_, event: &TableViewEvent, _| {
                    events.borrow_mut().push(event.clone());
                })
            }
        });
        table.update(cx, |table, cx| {
            table.set_contents(contents.clone(), cx);
            table.set_selection_enabled(true, cx);
            table.set_editing_enabled(true, cx);
            table.set_column_reordering_enabled(true, cx);
            table.move_column(AnyColumn(1), AnyColumn(0), cx);
            table.engine.applied_sorting = Some(AppliedSorting {
                col_idx: AnyColumn(1),
                direction: SortDirection::Asc,
            });
            table.apply_filter_sort(cx);
        });
        cx.condition(&table, |table, _| !table.mapping_pending)
            .await;
        assert!(
            events
                .borrow()
                .contains(&TableViewEvent::ColumnOrderChanged {
                    columns: vec!["name".into(), "id".into(), "age".into()],
                })
        );
        let first_cell = table.read_with(cx, |table, _| {
            let Some(bounds) = table.list_state.bounds_for_item(0) else {
                panic!("first row must be rendered");
            };
            gpui::point(bounds.left() + px(100.), bounds.center().y)
        });
        cx.simulate_mouse_down(first_cell, gpui::MouseButton::Right, Modifiers::default());
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("Ada".into())
        );
        cx.simulate_click(first_cell, Modifiers::default());
        assert_eq!(
            table.read_with(cx, |table, _| table.selected_cell()),
            Some((1, 1))
        );
        table.update_in(cx, |table, window, cx| {
            assert!(table.begin_cell_edit(1, 1, "Ada", window, cx));
        });
        cx.simulate_input("Updated");
        cx.simulate_keystrokes("enter");
        assert!(events.borrow().contains(&TableViewEvent::CellEdited {
            row: 1,
            column: 1,
            text: "Updated".into(),
        }));
        table.update(cx, |table, cx| {
            table.set_contents(contents, cx);
            assert_eq!(table.column_order(), ["name", "id", "age"]);
            table.set_column_order(&["age".into(), "age".into(), "missing".into()], cx);
            assert_eq!(table.column_order(), ["age", "id", "name"]);
            assert_eq!(
                table
                    .engine
                    .contents
                    .rows
                    .first()
                    .and_then(|row| row.get(AnyColumn(0)))
                    .and_then(TableCell::display_value)
                    .map(|value| value.as_ref()),
                Some("1"),
            );
        });
    }

    #[gpui::test(iterations = 5)]
    async fn row_selection_uses_data_indices_after_sort_and_filter(cx: &mut TestAppContext) {
        init_test(cx);
        let (table, cx) = cx.add_window_view(|window, cx| TableView::new(window, cx));
        table.update(cx, |table, cx| {
            table.set_contents(contents(&["C", "A", "D", "B"]), cx);
            table.engine.applied_sorting = Some(AppliedSorting {
                col_idx: AnyColumn(0),
                direction: SortDirection::Asc,
            });
            table.apply_filter_sort(cx);
        });
        cx.condition(&table, |table, _| {
            table.engine.d2d_mapping().get_data_row(DisplayRow(0)) == Some(DataRow(1))
        })
        .await;
        table.update_in(cx, |table, window, cx| {
            table.select_row(DataRow(3), Some(0), Modifiers::default(), window, cx);
            assert!(
                table.selected_rows().is_empty(),
                "CSV selection stays disabled"
            );
            assert!(!table.begin_cell_edit(3, 0, "B", window, cx));
            table.set_selection_enabled(true, cx);
            table.select_row(DataRow(3), Some(0), Modifiers::default(), window, cx);
            table.select_row(
                DataRow(2),
                Some(0),
                Modifiers {
                    shift: true,
                    ..Default::default()
                },
                window,
                cx,
            );
            assert_eq!(table.selected_rows(), vec![0, 2, 3]);
            table.select_row(
                DataRow(0),
                Some(0),
                Modifiers {
                    platform: cfg!(target_os = "macos"),
                    control: !cfg!(target_os = "macos"),
                    ..Default::default()
                },
                window,
                cx,
            );
            assert_eq!(table.selected_rows(), vec![2, 3]);
            table.engine.applied_sorting = Some(AppliedSorting {
                col_idx: AnyColumn(0),
                direction: SortDirection::Desc,
            });
            table.apply_filter_sort(cx);
        });
        cx.condition(&table, |table, _| {
            table.engine.d2d_mapping().get_data_row(DisplayRow(0)) == Some(DataRow(2))
        })
        .await;
        table.update(cx, |table, cx| {
            assert_eq!(table.selected_rows(), vec![2, 3]);
            table.toggle_filter(AnyColumn(0), Some("B".into()), cx);
        });
        cx.condition(&table, |table, _| {
            table.engine.d2d_mapping().visible_row_count() == 1
        })
        .await;
        table.update(cx, |table, cx| {
            assert_eq!(table.selected_rows(), vec![3]);
            table.select_all_visible(cx);
            assert_eq!(table.selected_rows(), vec![3]);
            table.clear_selection(cx);
            assert!(table.selected_rows().is_empty());
        });
    }

    #[gpui::test]
    async fn context_menu_preserves_selection_and_reports_data_coordinates(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let (table, cx) = cx.add_window_view(|window, cx| TableView::new(window, cx));
        cx.update(|window, _| window.activate_window());
        table.update(cx, |table, cx| {
            table.set_contents(contents(&["C", "A", "D", "B"]), cx);
            table.engine.applied_sorting = Some(AppliedSorting {
                col_idx: AnyColumn(0),
                direction: SortDirection::Asc,
            });
            table.apply_filter_sort(cx);
        });
        cx.condition(&table, |table, _| !table.mapping_pending)
            .await;
        let first_cell = table.read_with(cx, |table, _| {
            let Some(bounds) = table.list_state.bounds_for_item(0) else {
                panic!("first row must be rendered");
            };
            gpui::point(bounds.left() + px(100.), bounds.center().y)
        });
        cx.simulate_mouse_down(first_cell, gpui::MouseButton::Right, Modifiers::default());
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("A".into())
        );
        assert!(table.read_with(cx, |table, _| table.selected_rows().is_empty()));

        let events = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update({
            let events = events.clone();
            |_, cx| {
                cx.subscribe(&table, move |_, event: &TableViewEvent, _| {
                    events.borrow_mut().push(event.clone())
                })
            }
        });
        table.update(cx, |table, cx| {
            table.set_context_menu_enabled(true, cx);
            table.set_selected_rows([1, 3], cx);
        });
        cx.update(|_, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string("unchanged".into()))
        });
        events.borrow_mut().clear();
        cx.simulate_mouse_down(first_cell, gpui::MouseButton::Right, Modifiers::default());
        assert_eq!(
            table.read_with(cx, |table, _| table.selected_rows()),
            vec![1, 3]
        );
        assert_eq!(
            events.borrow().last(),
            Some(&TableViewEvent::ContextMenu {
                row: 1,
                column: Some(0),
                position: first_cell,
            })
        );
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("unchanged".into())
        );

        let row_identifier = table.read_with(cx, |table, _| {
            let Some(bounds) = table.list_state.bounds_for_item(2) else {
                panic!("third row must be rendered");
            };
            gpui::point(bounds.left() + px(20.), bounds.center().y)
        });
        cx.simulate_mouse_down(
            row_identifier,
            gpui::MouseButton::Right,
            Modifiers::default(),
        );
        assert_eq!(
            table.read_with(cx, |table, _| table.selected_rows()),
            vec![0]
        );
        assert_eq!(
            events.borrow().last(),
            Some(&TableViewEvent::ContextMenu {
                row: 0,
                column: None,
                position: row_identifier,
            })
        );

        table.update(cx, |table, cx| {
            table.toggle_filter(AnyColumn(0), Some("B".into()), cx)
        });
        cx.condition(&table, |table, _| !table.mapping_pending)
            .await;
        cx.simulate_mouse_down(first_cell, gpui::MouseButton::Right, Modifiers::default());
        assert_eq!(
            table.read_with(cx, |table, _| table.selected_rows()),
            vec![3]
        );
        assert_eq!(
            events.borrow().last(),
            Some(&TableViewEvent::ContextMenu {
                row: 3,
                column: Some(0),
                position: first_cell,
            })
        );
    }

    #[gpui::test]
    async fn reveal_waits_for_mapping_and_does_not_survive_replacement(cx: &mut TestAppContext) {
        init_test(cx);
        let (table, cx) = cx.add_window_view(|window, cx| TableView::new(window, cx));
        table.update(cx, |table, cx| {
            table.set_contents(contents(&["kept", "hidden"]), cx);
            table.toggle_filter(AnyColumn(0), Some("kept".into()), cx);
        });
        cx.condition(&table, |table, _| !table.mapping_pending)
            .await;
        let values = (0..1001).map(|row| format!("{row:04}")).collect::<Vec<_>>();
        table.update(cx, |table, cx| {
            table.set_contents(
                contents(&values.iter().map(String::as_str).collect::<Vec<_>>()),
                cx,
            );
            table.clear_all_filters(cx);
            table.reveal_row(1000, cx);
            assert!(table.pending_reveal.is_some());
        });
        cx.condition(&table, |table, _| !table.mapping_pending)
            .await;
        table.update(cx, |table, cx| {
            assert_eq!(table.engine.d2d_mapping().visible_row_count(), 1001);
            assert!(table.list_state.logical_scroll_top().item_ix > 0);
            assert!(table.pending_reveal.is_none());
            table.apply_filter_sort(cx);
            table.reveal_row(1000, cx);
            table.set_contents(contents(&["Replacement"]), cx);
            assert!(table.pending_reveal.is_none());
        });
        cx.condition(&table, |table, _| !table.mapping_pending)
            .await;
        table.read_with(cx, |table, _| {
            assert_eq!(table.list_state.item_count(), 1);
            assert_eq!(table.list_state.logical_scroll_top().item_ix, 0);
        });
    }

    #[gpui::test]
    async fn inline_edit_emits_data_cell_and_replacement_cancels_it(cx: &mut TestAppContext) {
        init_test(cx);
        let (table, cx) = cx.add_window_view(|window, cx| TableView::new(window, cx));
        cx.update(|window, _| window.activate_window());
        let events = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update({
            let events = events.clone();
            |_, cx| {
                cx.subscribe(&table, move |_, event: &TableViewEvent, _| {
                    events.borrow_mut().push(event.clone())
                })
            }
        });
        table.update(cx, |table, cx| {
            table.set_contents(contents(&["Grace", "Ada"]), cx);
            table.set_selection_enabled(true, cx);
            table.set_editing_enabled(true, cx);
        });
        cx.condition(&table, |table, _| {
            table.engine.d2d_mapping().visible_row_count() == 2
        })
        .await;
        table.update_in(cx, |table, window, cx| {
            assert!(table.begin_cell_edit(1, 0, "Ada", window, cx));
        });
        cx.simulate_input("Updated");
        cx.simulate_keystrokes("enter");
        assert_eq!(
            events.borrow().as_slice(),
            &[TableViewEvent::CellEdited {
                row: 1,
                column: 0,
                text: "Updated".into()
            }]
        );
        events.borrow_mut().clear();
        table.update_in(cx, |table, window, cx| {
            assert!(table.begin_cell_edit(1, 0, "Ada", window, cx));
        });
        cx.simulate_input("Cancelled");
        cx.simulate_keystrokes("escape");
        assert!(events.borrow().is_empty());
        table.update_in(cx, |table, window, cx| {
            assert!(table.begin_cell_edit(1, 0, "Blurred", window, cx));
        });
        cx.run_until_parked();
        table.update_in(cx, |table, window, cx| table.focus_handle.focus(window, cx));
        cx.run_until_parked();
        assert_eq!(
            events.borrow().as_slice(),
            &[TableViewEvent::CellEdited {
                row: 1,
                column: 0,
                text: "Blurred".into()
            }]
        );
        events.borrow_mut().clear();
        table.update_in(cx, |table, window, cx| {
            assert!(table.begin_cell_edit(1, 0, "Outside", window, cx));
        });
        cx.run_until_parked();
        cx.simulate_click(gpui::point(px(500.), px(400.)), Modifiers::default());
        assert_eq!(
            events.borrow().as_slice(),
            &[TableViewEvent::CellEdited {
                row: 1,
                column: 0,
                text: "Outside".into()
            }]
        );
        events.borrow_mut().clear();
        table.update_in(cx, |table, window, cx| {
            assert!(table.begin_cell_edit(1, 0, "Taken", window, cx));
            assert_eq!(table.take_cell_edit(cx), Some((1, 0, "Taken".into())));
        });
        cx.run_until_parked();
        assert!(
            events.borrow().is_empty(),
            "synchronous take must not emit a duplicate edit"
        );
        table.update_in(cx, |table, window, cx| {
            assert!(table.begin_cell_edit(1, 0, "Ada", window, cx));
            table.set_contents(contents(&["Replacement"]), cx);
            table.commit_cell_edit(window, cx);
            assert!(!table.begin_cell_edit(0, 0, "Replacement", window, cx));
            assert!(table.cell_edit.is_none());
        });
        cx.run_until_parked();
        assert!(
            events.borrow().is_empty(),
            "a replaced result must not emit an old edit"
        );
    }
}

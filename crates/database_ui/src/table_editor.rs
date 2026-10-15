use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use anyhow::{Result, anyhow};
use database_connection::{
    CancelQuery, CellInput, CellValue, ConnectionColor, DatabaseConnection, RowChange, TableInfo,
    TableSnapshot, cell_text,
};
use editor::Editor;
use gpui::{
    App, AppContext, ClipboardItem, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, Pixels, Point, Render, SharedString, Subscription, Task, Window,
    anchored, deferred,
};
use tabular_data_preview::{TableView, TableViewEvent};
use ui::{ContextMenu, ContextMenuEntry, DropdownMenu, prelude::*};
use util::ResultExt;
use workspace::{
    Item,
    item::{ItemEvent, SaveOptions},
};

use crate::{ROW_LIMIT, field, input, table_contents};

#[derive(Default)]
struct PendingChanges {
    updated: BTreeMap<usize, BTreeMap<usize, CellInput>>,
    deleted: BTreeSet<usize>,
    inserted: Vec<BTreeMap<usize, CellInput>>,
}

impl PendingChanges {
    fn is_empty(&self) -> bool {
        self.updated.is_empty() && self.deleted.is_empty() && self.inserted.is_empty()
    }

    fn count(&self, original_rows: usize) -> usize {
        self.updated
            .keys()
            .filter(|row| !self.deleted.contains(row))
            .count()
            + self
                .deleted
                .iter()
                .filter(|row| **row < original_rows)
                .count()
            + self
                .inserted
                .iter()
                .enumerate()
                .filter(|(index, _)| !self.deleted.contains(&(original_rows + index)))
                .count()
    }

    fn changes(&self, snapshot: &TableSnapshot) -> Result<Vec<RowChange>> {
        let original_rows = snapshot.result.values.len();
        let mut changes = Vec::new();
        for row in &self.deleted {
            if *row < original_rows {
                let original = snapshot
                    .result
                    .values
                    .get(*row)
                    .ok_or_else(|| anyhow!("The selected row is no longer available"))?;
                changes.push(RowChange::Delete {
                    original: original.clone(),
                });
            }
        }
        for (row, values) in &self.updated {
            if !self.deleted.contains(row) && !values.is_empty() {
                let original = snapshot
                    .result
                    .values
                    .get(*row)
                    .ok_or_else(|| anyhow!("The edited row is no longer available"))?;
                changes.push(RowChange::Update {
                    original: original.clone(),
                    values: values
                        .iter()
                        .map(|(column, value)| (*column, value.clone()))
                        .collect(),
                });
            }
        }
        for (index, values) in self.inserted.iter().enumerate() {
            if !self.deleted.contains(&(original_rows + index)) {
                changes.push(RowChange::Insert {
                    values: values
                        .iter()
                        .map(|(column, value)| (*column, value.clone()))
                        .collect(),
                });
            }
        }
        Ok(changes)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ValueMode {
    Text,
    Null,
    Default,
}

pub(super) struct DatabaseTableEditor {
    connection: DatabaseConnection,
    identity: crate::TableIdentity,
    title: SharedString,
    color: Option<ConnectionColor>,
    source: TableInfo,
    snapshot: Option<TableSnapshot>,
    pending: PendingChanges,
    table: Entity<TableView>,
    filter: Entity<Editor>,
    loaded_filter: String,
    offset: u64,
    page_size: u32,
    bulk_value: Entity<Editor>,
    bulk_column: usize,
    value_mode: ValueMode,
    focus_handle: FocusHandle,
    status: String,
    error: Option<String>,
    running: bool,
    schema_stale: bool,
    schema_generation: u64,
    cancel: Option<CancelQuery>,
    load_task: Option<Task<()>>,
    schema_refresh_task: Option<Task<()>>,
    write_task: Option<Task<()>>,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    _subscriptions: Vec<Subscription>,
}

impl DatabaseTableEditor {
    pub(super) fn new(
        connection: DatabaseConnection,
        title: String,
        source: TableInfo,
        color: Option<ConnectionColor>,
        connection_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let identity = crate::TableIdentity::new(connection_id.clone(), &source);
        let table = cx.new(|cx| TableView::new(window, cx));
        let observation = cx.observe(&table, |_, _, cx| {
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
        });
        let profile_events = crate::profile_events(cx);
        let appearance_subscription = cx.subscribe(
            &profile_events,
            move |this, _, event: &crate::ProfileAppearance, cx| {
                if event.id == connection_id && this.color != event.color {
                    this.color = event.color;
                    cx.emit(ItemEvent::UpdateTab);
                    cx.notify();
                }
            },
        );
        let table_events = crate::table_events(cx);
        let order_subscription = cx.subscribe(
            &table_events,
            |this, _, event: &crate::ColumnOrderChanged, cx| {
                if event.table == this.identity {
                    this.table.update(cx, |table, cx| {
                        table.set_column_order(&event.columns, cx);
                    });
                }
            },
        );
        let schema_subscription = cx.subscribe(
            &table_events,
            |this, _, event: &crate::TableSchemaChanged, cx| {
                if event.table == this.identity {
                    this.finish_cell_edit(cx);
                    this.schema_stale = true;
                    this.schema_generation = this.schema_generation.wrapping_add(1);
                    this.table.update(cx, |table, cx| {
                        table.set_editing_enabled(false, cx);
                    });
                    if this.pending.is_empty() {
                        this.refresh(cx);
                    } else {
                        this.error = Some(
                            "The table schema changed. Pending row changes are preserved but cannot be applied. Revert them to reload the table.".into(),
                        );
                    }
                    cx.notify();
                }
            },
        );
        let subscription =
            cx.subscribe_in(&table, window, |this, _, event, window, cx| match event {
                TableViewEvent::EditCell { row, column } => {
                    if !this.editable() || this.pending.deleted.contains(row) {
                        return;
                    }
                    this.bulk_column = *column;
                    let text = this
                        .cell_input(*row, *column)
                        .map(|value| match value {
                            CellInput::Text(text) => text,
                            CellInput::Null | CellInput::Default => String::new(),
                        })
                        .unwrap_or_default();
                    this.table.update(cx, |table, cx| {
                        table.begin_cell_edit(*row, *column, text, window, cx);
                    });
                }
                TableViewEvent::CellEdited { row, column, text } => {
                    if text.is_empty()
                        && matches!(
                            this.cell_input(*row, *column),
                            Some(CellInput::Null | CellInput::Default)
                        )
                    {
                        return;
                    }
                    this.stage_cell(*row, *column, CellInput::Text(text.clone()));
                    this.repaint(cx);
                    this.changed(cx);
                }
                TableViewEvent::SelectionChanged => cx.notify(),
                TableViewEvent::ColumnOrderChanged { columns } => {
                    crate::set_column_order(this.identity.clone(), columns.clone(), cx);
                }
                TableViewEvent::ContextMenu {
                    row,
                    column,
                    position,
                } => {
                    let (row, column, position) = (*row, *column, *position);
                    cx.defer_in(window, move |this, window, cx| {
                        this.deploy_context_menu(row, column, position, window, cx);
                    });
                }
            });
        cx.on_release(|this, cx| {
            if let Some(cancel) = this.cancel.take() {
                cx.background_spawn(async move {
                    cancel.cancel().log_err();
                })
                .detach();
            }
        })
        .detach();
        Self {
            connection,
            identity,
            title: title.into(),
            color,
            source,
            snapshot: None,
            pending: PendingChanges::default(),
            table,
            filter: input(
                "SQL WHERE expression, e.g. status = 'pending'",
                "",
                false,
                window,
                cx,
            ),
            loaded_filter: String::new(),
            offset: 0,
            page_size: ROW_LIMIT,
            bulk_value: input("Value for selected rows", "", false, window, cx),
            bulk_column: 0,
            value_mode: ValueMode::Text,
            focus_handle: cx.focus_handle(),
            status: "Loading…".into(),
            error: None,
            running: false,
            schema_stale: false,
            schema_generation: 0,
            cancel: None,
            load_task: None,
            schema_refresh_task: None,
            write_task: None,
            context_menu: None,
            _subscriptions: vec![
                subscription,
                observation,
                appearance_subscription,
                order_subscription,
                schema_subscription,
            ],
        }
    }

    fn original_rows(&self) -> usize {
        self.snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.result.values.len())
    }

    fn pending_count(&self) -> usize {
        self.pending.count(self.original_rows())
    }

    fn editable(&self) -> bool {
        !self.running
            && !self.schema_stale
            && self
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.editability_error.is_none())
    }

    fn changed(&self, cx: &mut Context<Self>) {
        cx.emit(ItemEvent::Edit);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    pub(super) fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.schema_stale && !self.running && self.pending.is_empty() {
            self.refresh_changed_schema(cx);
            return;
        }
        self.refresh_page(self.offset, self.page_size, cx);
    }

    fn refresh_changed_schema(&mut self, cx: &mut Context<Self>) {
        let generation = self.schema_generation;
        self.status = "Waiting to reload changed schema…".into();
        self.error = None;
        self.schema_refresh_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let waiting = this
                    .update(cx, |this, cx| {
                        if generation != this.schema_generation
                            || !this.schema_stale
                            || !this.pending.is_empty()
                        {
                            return false;
                        }
                        if this.running || this.connection.is_busy() {
                            return true;
                        }
                        this.refresh_page(this.offset, this.page_size, cx);
                        false
                    })
                    .log_err();
                if waiting != Some(true) {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
            }
        }));
        cx.notify();
    }

    fn refresh_page(&mut self, requested_offset: u64, page_size: u32, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        self.finish_cell_edit(cx);
        if self.pending_count() > 0 {
            self.error = Some(
                "Apply or revert pending changes before refreshing or changing the filter.".into(),
            );
            cx.notify();
            return;
        }
        let filter = self.filter.read(cx).text(cx).trim().to_owned();
        let offset = if filter != self.loaded_filter {
            0
        } else {
            requested_offset
        };
        let operation = match self.connection.start_browse_filtered(
            &self.source,
            Some(&filter),
            page_size,
            offset,
        ) {
            Ok(operation) => operation,
            Err(error) => {
                self.error = Some(format!("{error:#}"));
                cx.notify();
                return;
            }
        };
        self.running = true;
        self.cancel = Some(operation.cancel_handle());
        self.error = None;
        self.status = "Loading…".into();
        self.table.update(cx, |table, cx| {
            table.set_editing_enabled(false, cx);
            table.set_loading(true, cx);
        });
        let schema_generation = self.schema_generation;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { operation.execute() })
                .await;
            this.update(cx, |this, cx| {
                this.running = false;
                this.cancel = None;
                if schema_generation != this.schema_generation {
                    this.table
                        .update(cx, |table, cx| table.set_loading(false, cx));
                    this.refresh(cx);
                    return;
                }
                match result {
                    Ok(snapshot) => {
                        this.schema_stale = false;
                        this.offset = snapshot.offset;
                        this.page_size = page_size;
                        let page = snapshot.offset / u64::from(page_size) + 1;
                        let pages = snapshot.total_rows.div_ceil(u64::from(page_size)).max(1);
                        let row_count = snapshot.result.rows.len() as u64;
                        let first_row = if row_count == 0 {
                            0
                        } else {
                            snapshot.offset + 1
                        };
                        let last_row = if row_count == 0 {
                            0
                        } else {
                            snapshot.offset.saturating_add(row_count)
                        };
                        this.status = format!(
                            "Rows {first_row}–{last_row} of {} · Page {page} of {pages}",
                            snapshot.total_rows,
                        );
                        this.loaded_filter = filter;
                        this.snapshot = Some(snapshot);
                        this.pending = PendingChanges::default();
                        this.table.update(cx, |table, cx| table.clear_selection(cx));
                        this.repaint(cx);
                    }
                    Err(error) => {
                        this.error = Some(format!("{error:#}"));
                        this.status = "Load failed".into();
                        // A failed refresh must not leave an old snapshot available for edits.
                        this.snapshot = None;
                        this.table.update(cx, |table, cx| {
                            table.set_contents(Default::default(), cx);
                        });
                    }
                }
                this.table
                    .update(cx, |table, cx| table.set_loading(false, cx));
                this.changed(cx);
            })
            .log_err();
        }));
        cx.notify();
    }

    fn cell_input(&self, row: usize, column: usize) -> Option<CellInput> {
        if row >= self.original_rows() {
            return self
                .pending
                .inserted
                .get(row - self.original_rows())
                .map(|values| values.get(&column).cloned().unwrap_or(CellInput::Default));
        }
        if let Some(value) = self
            .pending
            .updated
            .get(&row)
            .and_then(|values| values.get(&column))
        {
            return Some(value.clone());
        }
        self.snapshot
            .as_ref()?
            .result
            .values
            .get(row)?
            .get(column)
            .map(|value| {
                if matches!(value, CellValue::Null) {
                    CellInput::Null
                } else {
                    CellInput::Text(cell_text(value))
                }
            })
    }

    fn stage_cell(&mut self, row: usize, column: usize, value: CellInput) {
        if !self.editable() || self.pending.deleted.contains(&row) {
            return;
        }
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        if column >= snapshot.columns.len() {
            return;
        }
        let original_rows = snapshot.result.values.len();
        if row >= original_rows {
            if let Some(values) = self.pending.inserted.get_mut(row - original_rows) {
                if matches!(value, CellInput::Default) {
                    values.remove(&column);
                } else {
                    values.insert(column, value);
                }
            }
        } else if let Some(original) = snapshot
            .result
            .values
            .get(row)
            .and_then(|row| row.get(column))
        {
            let unchanged = match &value {
                CellInput::Text(text) => {
                    !matches!(original, CellValue::Null) && *text == cell_text(original)
                }
                CellInput::Null => matches!(original, CellValue::Null),
                CellInput::Default => false,
            };
            if unchanged {
                if let Some(values) = self.pending.updated.get_mut(&row) {
                    values.remove(&column);
                    if values.is_empty() {
                        self.pending.updated.remove(&row);
                    }
                }
            } else {
                self.pending
                    .updated
                    .entry(row)
                    .or_default()
                    .insert(column, value);
            }
        }
    }

    fn repaint(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let mut result = snapshot.result.clone();
        let mut modified = Vec::new();
        for (row, values) in &self.pending.updated {
            if let Some(cells) = result.rows.get_mut(*row) {
                for (column, value) in values {
                    if let Some(cell) = cells.get_mut(*column) {
                        *cell = input_label(value);
                        modified.push((*row, *column));
                    }
                }
            }
        }
        for (index, values) in self.pending.inserted.iter().enumerate() {
            result.rows.push(
                (0..snapshot.columns.len())
                    .map(|column| {
                        modified.push((self.original_rows() + index, column));
                        values
                            .get(&column)
                            .map(input_label)
                            .unwrap_or_else(|| "DEFAULT".into())
                    })
                    .collect(),
            );
        }
        let editable = self.editable();
        let column_order = crate::column_order(&self.identity, cx);
        let selected = self.table.read(cx).selected_rows();
        let reveal = selected.first().copied();
        self.table.update(cx, |table, cx| {
            table.set_contents(table_contents(result), cx);
            table.set_column_order(&column_order, cx);
            table.set_column_reordering_enabled(true, cx);
            table.set_selection_enabled(true, cx);
            table.set_context_menu_enabled(true, cx);
            table.set_editing_enabled(editable, cx);
            table.set_selected_rows(selected, cx);
            table.set_modified_cells(modified, cx);
            table.set_deleted_rows(self.pending.deleted.iter().copied(), cx);
            if let Some(row) = reveal {
                table.reveal_row(row, cx);
            }
        });
    }

    fn add_row(&mut self, cx: &mut Context<Self>) {
        if !self.editable() {
            return;
        }
        let row = self.original_rows() + self.pending.inserted.len();
        self.pending.inserted.push(BTreeMap::new());
        self.repaint(cx);
        self.table.update(cx, |table, cx| {
            table.clear_all_filters(cx);
            table.set_selected_rows([row], cx);
            table.reveal_row(row, cx);
        });
        self.changed(cx);
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        if !self.editable() {
            return;
        }
        let original_rows = self.original_rows();
        let selected = self.table.read(cx).selected_rows();
        for row in selected.iter().rev() {
            if *row < original_rows {
                self.pending.deleted.insert(*row);
            } else {
                let index = *row - original_rows;
                if index < self.pending.inserted.len() {
                    self.pending.inserted.remove(index);
                }
            }
        }
        self.repaint(cx);
        self.table.update(cx, |table, cx| {
            table.set_selected_rows(selected.into_iter().filter(|row| *row < original_rows), cx)
        });
        self.changed(cx);
    }

    fn set_selected_value(&mut self, cx: &mut Context<Self>) {
        if !self.editable() {
            return;
        }
        let value = match self.value_mode {
            ValueMode::Text => CellInput::Text(self.bulk_value.read(cx).text(cx)),
            ValueMode::Null => CellInput::Null,
            ValueMode::Default => CellInput::Default,
        };
        for row in self.table.read(cx).selected_rows() {
            self.stage_cell(row, self.bulk_column, value.clone());
        }
        self.repaint(cx);
        self.changed(cx);
    }

    fn revert(&mut self, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        self.pending = PendingChanges::default();
        self.error = None;
        if self.schema_stale {
            self.refresh(cx);
        } else {
            self.repaint(cx);
        }
        self.changed(cx);
    }

    fn save_changes(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let operation = (|| {
            anyhow::ensure!(self.editable(), "This table cannot be edited right now");
            let snapshot = self
                .snapshot
                .as_ref()
                .ok_or_else(|| anyhow!("Load a table first"))?;
            let changes = self.pending.changes(snapshot)?;
            self.connection.start_changes(snapshot, changes)
        })();
        let operation = match operation {
            Ok(operation) => operation,
            Err(error) => {
                self.error = Some(format!("{error:#}"));
                cx.notify();
                return Task::ready(Err(error));
            }
        };
        self.running = true;
        self.cancel = Some(operation.cancel_handle());
        self.error = None;
        self.status = "Saving changes…".into();
        self.table
            .update(cx, |table, cx| table.set_editing_enabled(false, cx));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { operation.execute() })
                .await;
            this.update(cx, |this, cx| {
                this.running = false;
                this.cancel = None;
                match &result {
                    Ok(result) => {
                        this.pending = PendingChanges::default();
                        this.status = format!("Saved {} rows", result.affected_rows);
                        this.snapshot = None;
                        this.table
                            .update(cx, |table, cx| table.set_contents(Default::default(), cx));
                        this.changed(cx);
                        this.refresh(cx);
                    }
                    Err(error) => {
                        this.status = "Save failed · changes remain pending".into();
                        this.error = Some(format!("{error:#}"));
                        this.repaint(cx);
                        cx.notify();
                    }
                }
            })?;
            result.map(|_| ())
        })
    }

    fn apply(&mut self, cx: &mut Context<Self>) {
        self.finish_cell_edit(cx);
        if !self.editable() || self.pending_count() == 0 {
            return;
        }
        let task = self.save_changes(cx);
        self.write_task = Some(cx.spawn(async move |_, _| {
            task.await.log_err();
        }));
    }

    fn finish_cell_edit(&mut self, cx: &mut Context<Self>) {
        let Some((row, column, text)) = self.table.update(cx, |table, cx| table.take_cell_edit(cx))
        else {
            return;
        };
        if !(text.is_empty()
            && matches!(
                self.cell_input(row, column),
                Some(CellInput::Null | CellInput::Default)
            ))
        {
            self.stage_cell(row, column, CellInput::Text(text));
        }
        self.repaint(cx);
        self.changed(cx);
    }

    fn cancel_operation(&mut self, cx: &mut Context<Self>) {
        let Some(cancel) = self.cancel.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            if let Err(error) = cx.background_spawn(async move { cancel.cancel() }).await {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("Cancel failed: {error:#}"));
                    cx.notify();
                })
                .log_err();
            }
        })
        .detach();
    }

    fn deploy_context_menu(
        &mut self,
        row: usize,
        column: Option<usize>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.finish_cell_edit(cx);
        let editable = self.editable();
        let selected = self.table.read(cx).selected_rows();
        let has_selected_rows = selected
            .iter()
            .any(|row| !self.pending.deleted.contains(row));
        let copy_text = column
            .and_then(|column| self.cell_input(row, column))
            .map(|value| input_label(&value));
        let nullable_column = column.filter(|column| {
            self.snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.columns.get(*column))
                .is_some_and(|column| column.nullable)
        });
        let this = cx.entity().downgrade();
        let menu_context = self.table.focus_handle(cx);
        let menu = ContextMenu::build(window, cx, |menu, _, _| {
            menu.context(menu_context)
                .item(
                    ContextMenuEntry::new("Copy cell")
                        .disabled(copy_text.is_none())
                        .handler(move |_, cx| {
                            if let Some(text) = &copy_text {
                                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                            }
                        }),
                )
                .separator()
                .item(
                    ContextMenuEntry::new("Add row")
                        .disabled(!editable)
                        .handler({
                            let this = this.clone();
                            move |_, cx| {
                                this.update(cx, |this, cx| this.add_row(cx)).log_err();
                            }
                        }),
                )
                .item(
                    ContextMenuEntry::new(format!("Delete selected ({})", selected.len()))
                        .disabled(!editable || !has_selected_rows)
                        .handler({
                            let this = this.clone();
                            move |_, cx| {
                                this.update(cx, |this, cx| this.delete_selected(cx))
                                    .log_err();
                            }
                        }),
                )
                .item(
                    ContextMenuEntry::new("Set column to NULL in selected rows")
                        .disabled(!editable || !has_selected_rows || nullable_column.is_none())
                        .handler({
                            let this = this.clone();
                            move |_, cx| {
                                if let Some(column) = nullable_column {
                                    this.update(cx, |this, cx| {
                                        this.bulk_column = column;
                                        this.value_mode = ValueMode::Null;
                                        this.set_selected_value(cx);
                                    })
                                    .log_err();
                                }
                            }
                        }),
                )
                .separator()
                .item(
                    ContextMenuEntry::new(format!("Apply ({})", self.pending_count()))
                        .disabled(!editable || self.pending_count() == 0)
                        .handler({
                            let this = this.clone();
                            move |_, cx| {
                                this.update(cx, |this, cx| this.apply(cx)).log_err();
                            }
                        }),
                )
                .item(
                    ContextMenuEntry::new("Revert")
                        .disabled(self.running || self.pending.is_empty())
                        .handler({
                            let this = this.clone();
                            move |_, cx| {
                                this.update(cx, |this, cx| this.revert(cx)).log_err();
                            }
                        }),
                )
                .item(
                    ContextMenuEntry::new("Refresh")
                        .disabled(self.running || self.pending_count() > 0)
                        .handler(move |_, cx| {
                            this.update(cx, |this, cx| this.refresh(cx)).log_err();
                        }),
                )
        });
        window.focus(&menu.focus_handle(cx), cx);
        let subscription =
            cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, window, cx| {
                if this
                    .context_menu
                    .as_ref()
                    .is_some_and(|(menu, _, _)| menu.focus_handle(cx).contains_focused(window, cx))
                {
                    window.focus(&this.table.focus_handle(cx), cx);
                }
                this.context_menu = None;
                cx.notify();
            });
        self.context_menu = Some((menu, position, subscription));
        cx.notify();
    }
}

fn input_label(value: &CellInput) -> String {
    match value {
        CellInput::Text(text) => text.clone(),
        CellInput::Null => "NULL".into(),
        CellInput::Default => "DEFAULT".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use database_connection::{ConnectionConfig, DatabaseKind, Secrets};

    #[test]
    fn staged_changes_preserve_row_identity_and_cancelled_inserts() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("staging.sqlite");
        rusqlite::Connection::open(&path)?.execute_batch(
            "CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT DEFAULT 'default');
             INSERT INTO items VALUES (1, 'one'), (2, 'two'), (3, 'three');",
        )?;
        let connection = DatabaseConnection::connect(
            &ConnectionConfig {
                kind: DatabaseKind::Sqlite,
                path: Some(path),
                ..Default::default()
            },
            Secrets::default(),
        )?;
        let table = TableInfo {
            schema: None,
            name: "items".into(),
            is_view: false,
        };
        let snapshot = connection.start_browse(&table, 100, 0)?.execute()?;
        let mut pending = PendingChanges::default();
        pending.updated.insert(
            0,
            BTreeMap::from([(1, CellInput::Text("discarded".into()))]),
        );
        pending
            .updated
            .insert(1, BTreeMap::from([(1, CellInput::Null)]));
        pending.deleted.insert(0);
        pending
            .inserted
            .push(BTreeMap::from([(1, CellInput::Text(String::new()))]));
        pending
            .inserted
            .push(BTreeMap::from([(1, CellInput::Text("cancelled".into()))]));
        pending.deleted.insert(4);
        assert_eq!(pending.count(3), 3);
        let changes = pending.changes(&snapshot)?;
        assert_eq!(changes.len(), 3);
        assert!(
            matches!(changes.first(), Some(RowChange::Delete { original }) if original.first() == Some(&CellValue::Int(1)))
        );
        assert!(
            matches!(changes.get(1), Some(RowChange::Update { original, values }) if original.first() == Some(&CellValue::Int(2)) && values == &vec![(1, CellInput::Null)])
        );
        assert!(
            matches!(changes.get(2), Some(RowChange::Insert { values }) if values == &vec![(1, CellInput::Text(String::new()))])
        );
        assert_eq!(
            connection
                .start_changes(&snapshot, changes)?
                .execute()?
                .affected_rows,
            3
        );
        let rows = connection
            .start_browse(&table, 100, 0)?
            .execute()?
            .result
            .values;
        assert_eq!(
            rows,
            vec![
                vec![CellValue::Int(2), CellValue::Null],
                vec![CellValue::Int(3), CellValue::Text("three".into())],
                vec![CellValue::Int(4), CellValue::Text(String::new())]
            ]
        );
        connection.disconnect()?;
        Ok(())
    }
}

impl Focusable for DatabaseTableEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
impl EventEmitter<ItemEvent> for DatabaseTableEditor {}
impl Item for DatabaseTableEditor {
    type Event = ItemEvent;
    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        self.title.clone()
    }
    fn tab_content(
        &self,
        params: workspace::item::TabContentParams,
        _: &Window,
        cx: &App,
    ) -> gpui::AnyElement {
        h_flex()
            .gap_1()
            .child(
                Icon::new(IconName::Table)
                    .size(IconSize::Small)
                    .color(crate::connection_color(self.color)),
            )
            .child(
                Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
                    .single_line()
                    .color(params.text_color()),
            )
            .into_any_element()
    }
    fn to_item_events(event: &Self::Event, emit: &mut dyn FnMut(ItemEvent)) {
        emit(*event);
    }
    fn is_dirty(&self, cx: &App) -> bool {
        self.pending_count() > 0
            || self
                .table
                .read(cx)
                .pending_cell_edit(cx)
                .is_some_and(|(row, column, text)| match self.cell_input(row, column) {
                    Some(CellInput::Text(original)) => text != original,
                    Some(CellInput::Null | CellInput::Default) => !text.is_empty(),
                    None => false,
                })
    }
    fn can_save(&self, _: &App) -> bool {
        self.editable()
    }
    fn save(
        &mut self,
        options: SaveOptions,
        _: Entity<project::Project>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        // Editor autosave must never turn a staged database change into a write.
        if options.autosave {
            return Task::ready(Ok(()));
        }
        self.finish_cell_edit(cx);
        if self.pending_count() == 0 {
            Task::ready(Ok(()))
        } else {
            self.save_changes(cx)
        }
    }
    fn reload(
        &mut self,
        _: Entity<project::Project>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.revert(cx);
        Task::ready(Ok(()))
    }
}

impl Render for DatabaseTableEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pending = self.pending_count();
        let selected = self.table.read(cx).selected_rows().len();
        let editable = self.editable();
        let last_page_offset = self.snapshot.as_ref().map(|snapshot| {
            let page_size = u64::from(self.page_size);
            snapshot.total_rows.saturating_sub(1) / page_size * page_size
        });
        let can_go_back =
            !self.running && pending == 0 && self.snapshot.is_some() && self.offset > 0;
        let can_go_forward = !self.running
            && pending == 0
            && last_page_offset.is_some_and(|last_offset| self.offset < last_offset);
        let columns: Vec<_> = self
            .snapshot
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .columns
                    .iter()
                    .map(|column| column.name.clone())
                    .collect()
            })
            .unwrap_or_default();
        let column_label = columns
            .get(self.bulk_column)
            .cloned()
            .unwrap_or_else(|| "Column".into());
        let this = cx.entity().downgrade();
        let column_menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            for (column, name) in columns.into_iter().enumerate() {
                let this = this.clone();
                menu = menu.entry(name, None, move |_, cx| {
                    this.update(cx, |this, cx| {
                        this.bulk_column = column;
                        cx.notify();
                    })
                    .log_err();
                });
            }
            menu
        });
        let this = cx.entity().downgrade();
        let page_size_menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            for page_size in [
                100,
                500,
                1000,
                2000,
                5000,
                database_connection::MAX_ROW_LIMIT,
            ] {
                let this = this.clone();
                menu = menu.entry(format!("{page_size} rows"), None, move |_, cx| {
                    this.update(cx, |this, cx| this.refresh_page(0, page_size, cx))
                        .log_err();
                });
            }
            menu
        });
        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .when_some(self.color, |view, color| {
                let color = crate::connection_color(Some(color));
                view.child(
                    h_flex()
                        .px_2()
                        .py_1()
                        .gap_2()
                        .min_w_0()
                        .flex_shrink_0()
                        .border_t_2()
                        .border_color(color.color(cx))
                        .bg(color.color(cx).opacity(0.1))
                        .child(
                            Icon::new(IconName::Table)
                                .size(IconSize::Small)
                                .color(color),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Label::new(self.title.clone()).truncate().color(color)),
                        ),
                )
            })
            .child(
                h_flex()
                    .p_2()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        Button::new("refresh-table", "Refresh")
                            .disabled(self.running || pending > 0)
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    )
                    .child(
                        Button::new("add-table-row", "Add row")
                            .disabled(!editable)
                            .on_click(cx.listener(|this, _, _, cx| this.add_row(cx))),
                    )
                    .child(
                        Button::new("delete-table-rows", format!("Delete selected ({selected})"))
                            .disabled(!editable || selected == 0)
                            .on_click(cx.listener(|this, _, _, cx| this.delete_selected(cx))),
                    )
                    .child(
                        Button::new("apply-table-changes", format!("Apply ({pending})"))
                            .disabled(
                                !editable
                                    || (pending == 0 && !self.table.read(cx).is_cell_editing()),
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.apply(cx))),
                    )
                    .child(
                        Button::new("revert-table-changes", "Revert")
                            .disabled(self.running || self.pending.is_empty())
                            .on_click(cx.listener(|this, _, _, cx| this.revert(cx))),
                    )
                    .child(
                        Button::new("cancel-table-operation", "Cancel")
                            .disabled(!self.running)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_operation(cx))),
                    ),
            )
            .child(
                h_flex()
                    .px_2()
                    .pb_2()
                    .gap_2()
                    .items_end()
                    .child(field("WHERE", self.filter.clone(), cx))
                    .child(
                        Button::new("filter-table", "Filter")
                            .disabled(self.running || pending > 0)
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    ),
            )
            .child(
                h_flex()
                    .px_2()
                    .pb_2()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        Button::new("select-table-rows", "Select page")
                            .disabled(self.running)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.table
                                    .update(cx, |table, cx| table.select_all_visible(cx));
                            })),
                    )
                    .child(
                        Button::new("clear-table-selection", "Clear selection")
                            .disabled(self.running)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.table.update(cx, |table, cx| table.clear_selection(cx));
                            })),
                    )
                    .child(Label::new("Set").color(Color::Muted))
                    .child(
                        DropdownMenu::new("bulk-column", column_label, column_menu)
                            .disabled(!editable),
                    )
                    .child(
                        Button::new(
                            "bulk-value-mode",
                            match self.value_mode {
                                ValueMode::Text => "Value",
                                ValueMode::Null => "NULL",
                                ValueMode::Default => "DEFAULT",
                            },
                        )
                        .disabled(!editable)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.value_mode = match this.value_mode {
                                ValueMode::Text => ValueMode::Null,
                                ValueMode::Null => ValueMode::Default,
                                ValueMode::Default => ValueMode::Text,
                            };
                            cx.notify();
                        })),
                    )
                    .when(self.value_mode == ValueMode::Text, |view| {
                        view.child(
                            div()
                                .w_48()
                                .p_1()
                                .border_1()
                                .border_color(cx.theme().colors().border)
                                .child(self.bulk_value.clone()),
                        )
                    })
                    .child(
                        Button::new("set-selected-value", "Set selected")
                            .disabled(!editable || selected == 0)
                            .on_click(cx.listener(|this, _, _, cx| this.set_selected_value(cx))),
                    ),
            )
            .when_some(
                self.snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.editability_error.clone()),
                |view, reason| {
                    view.child(
                        div()
                            .px_2()
                            .pb_2()
                            .child(Label::new(reason).color(Color::Muted)),
                    )
                },
            )
            .when_some(self.error.clone(), |view, error| {
                view.child(div().p_2().child(Label::new(error).color(Color::Error)))
            })
            .child(div().flex_1().min_h_0().child(self.table.clone()))
            .child(
                h_flex()
                    .flex_none()
                    .p_2()
                    .gap_2()
                    .flex_wrap()
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .child(Label::new(self.status.clone()).color(Color::Muted))
                    .child(
                        DropdownMenu::new(
                            "table-page-size",
                            format!("{} per page", self.page_size),
                            page_size_menu,
                        )
                        .disabled(self.running || pending > 0),
                    )
                    .child(
                        Button::new("first-table-page", "First")
                            .disabled(!can_go_back)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.refresh_page(0, this.page_size, cx);
                            })),
                    )
                    .child(
                        Button::new("previous-table-page", "Previous")
                            .disabled(!can_go_back)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.refresh_page(
                                    this.offset.saturating_sub(this.page_size.into()),
                                    this.page_size,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Button::new("next-table-page", "Next")
                            .disabled(!can_go_forward)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.refresh_page(
                                    this.offset.saturating_add(this.page_size.into()),
                                    this.page_size,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Button::new("last-table-page", "Last")
                            .disabled(!can_go_forward)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(offset) = last_page_offset {
                                    this.refresh_page(offset, this.page_size, cx);
                                }
                            })),
                    )
                    .child(
                        Label::new(
                            "Double-click to edit · Enter to stage · ⌘/Shift-click to select",
                        )
                        .color(Color::Muted),
                    ),
            )
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(gpui::Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
    }
}

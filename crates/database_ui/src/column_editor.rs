use anyhow::{Result, anyhow, ensure};
use database_connection::{
    CancelQuery, ColumnMetadata, DatabaseConnection, TableInfo, TableStructure,
};
use editor::{Editor, EditorEvent};
use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render,
    SharedString, Subscription, Task, Window,
};
use ui::prelude::*;
use util::ResultExt;
use workspace::{
    Item,
    item::{ItemEvent, SaveOptions},
};

use crate::{TableIdentity, field, input, table_label};

pub(super) struct ColumnEditor {
    connection: DatabaseConnection,
    identity: TableIdentity,
    table: TableInfo,
    structure: Option<TableStructure>,
    column: ColumnMetadata,
    name: Entity<Editor>,
    type_name: Entity<Editor>,
    sql: Entity<Editor>,
    preview: Option<(String, String)>,
    running: bool,
    status: Option<String>,
    error: Option<String>,
    cancel: Option<CancelQuery>,
    task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ColumnEditor {
    pub(super) fn new(
        connection_id: String,
        connection: DatabaseConnection,
        structure: TableStructure,
        column: ColumnMetadata,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = input("Column name", &column.name, false, window, cx);
        let type_name = input("SQL type", &column.type_name, false, window, cx);
        let sql = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_read_only(true);
            editor
        });
        let subscriptions = [&name, &type_name]
            .into_iter()
            .map(|editor| {
                cx.subscribe(editor, |this, _, event, cx| {
                    if matches!(event, EditorEvent::BufferEdited) {
                        this.preview = None;
                        this.status = None;
                        this.error = None;
                        cx.emit(ItemEvent::Edit);
                        cx.emit(ItemEvent::UpdateTab);
                        cx.notify();
                    }
                })
            })
            .collect();
        cx.on_release(|this, cx| {
            if let Some(cancel) = this.cancel.take() {
                cx.background_spawn(async move {
                    cancel.cancel().log_err();
                })
                .detach();
            }
        })
        .detach();
        let editor = Self {
            identity: TableIdentity::new(connection_id, &structure.table),
            table: structure.table.clone(),
            connection,
            structure: Some(structure),
            column,
            name,
            type_name,
            sql,
            preview: None,
            running: false,
            status: None,
            error: None,
            cancel: None,
            task: None,
            _subscriptions: subscriptions,
        };
        editor.update_read_only(cx);
        editor
    }

    fn editable(&self) -> bool {
        !self.running
            && !self.connection.is_read_only()
            && self
                .structure
                .as_ref()
                .is_some_and(|structure| structure.editability_error.is_none())
    }

    fn update_read_only(&self, cx: &mut App) {
        let read_only = !self.editable();
        self.name
            .update(cx, |editor, _| editor.set_read_only(read_only));
        self.type_name.update(cx, |editor, _| {
            editor.set_read_only(read_only || !self.connection.supports_column_type_changes());
        });
    }

    fn draft(&self, cx: &App) -> (String, String) {
        (
            self.name.read(cx).text(cx),
            self.type_name.read(cx).text(cx).trim().to_owned(),
        )
    }

    fn preview_sql(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Result<()> {
        let result = (|| {
            ensure!(self.editable(), "This column cannot be edited right now");
            let structure = self
                .structure
                .as_ref()
                .ok_or_else(|| anyhow!("Reload the column before editing it"))?;
            let (name, type_name) = self.draft(cx);
            let statements = self.connection.column_change_sql(
                structure,
                &self.column.name,
                &name,
                &type_name,
            )?;
            ensure!(!statements.is_empty(), "There are no pending changes");
            self.sql.update(cx, |editor, cx| {
                editor.set_text(statements.join("\n"), window, cx);
            });
            self.preview = Some((name, type_name));
            self.error = None;
            self.status = None;
            Ok(())
        })();
        if let Err(error) = &result {
            self.preview = None;
            self.error = Some(format!("{error:#}"));
        }
        cx.notify();
        result
    }

    fn revert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        self.name.update(cx, |editor, cx| {
            editor.set_text(self.column.name.clone(), window, cx);
        });
        self.type_name.update(cx, |editor, cx| {
            editor.set_text(self.column.type_name.clone(), window, cx);
        });
        self.preview = None;
        self.error = None;
        self.status = None;
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn accept_structure(
        &mut self,
        structure: TableStructure,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let column = structure
            .columns
            .iter()
            .find(|column| column.name == self.column.name)
            .cloned()
            .ok_or_else(|| {
                anyhow!("This column no longer exists; reopen it from the database tree")
            })?;
        self.column = column;
        self.structure = Some(structure);
        self.revert(window, cx);
        Ok(())
    }

    fn save_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Task<Result<()>> {
        let operation = (|| {
            ensure!(self.editable(), "This column cannot be edited right now");
            ensure!(
                self.preview.as_ref() == Some(&self.draft(cx)),
                "Preview the current SQL changes before applying them"
            );
            let structure = self
                .structure
                .as_ref()
                .ok_or_else(|| anyhow!("Reload the column before editing it"))?;
            let (name, type_name) = self.draft(cx);
            self.connection
                .start_column_change(structure, &self.column.name, &name, &type_name)
        })();
        let operation = match operation {
            Ok(operation) => operation,
            Err(error) => {
                self.error = Some(format!("{error:#}"));
                cx.notify();
                return Task::ready(Err(error));
            }
        };
        let (name, type_name) = self.draft(cx);
        let old_name = self.column.name.clone();
        let connection = self.connection.clone();
        let table = self.table.clone();
        self.cancel = Some(operation.cancel_handle());
        self.running = true;
        self.error = None;
        self.status = Some("Applying column changes…".into());
        self.update_read_only(cx);
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    operation.execute()?;
                    Ok::<_, anyhow::Error>(
                        connection
                            .start_table_structure(&table)
                            .and_then(|operation| operation.execute()),
                    )
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.running = false;
                this.cancel = None;
                match &result {
                    Ok(refreshed) => {
                        this.column.name = name.clone();
                        this.column.type_name = type_name;
                        this.structure = None;
                        this.preview = None;
                        let refresh_result = match refreshed {
                            Ok(structure) => this.accept_structure(structure.clone(), window, cx),
                            Err(error) => Err(anyhow!("{error:#}")),
                        };
                        if let Err(error) = refresh_result {
                            this.error = Some(format!(
                                "Column changes were applied, but metadata could not be refreshed: {error:#}"
                            ));
                        }
                        this.status = Some("Column changes applied".into());
                        let renamed = (old_name != name).then_some((old_name, name));
                        crate::table_schema_changed(
                            this.identity.clone(),
                            renamed,
                            this.structure.clone(),
                            cx,
                        );
                    }
                    Err(error) => {
                        this.preview = None;
                        this.status = Some("Could not apply column changes".into());
                        this.error = Some(format!("{error:#}"));
                    }
                }
                this.update_read_only(cx);
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            })?;
            result.map(|_| ())
        })
    }

    fn reload_column(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.running || self.is_dirty(cx) {
            return;
        }
        let operation = match self.connection.start_table_structure(&self.table) {
            Ok(operation) => operation,
            Err(error) => {
                self.error = Some(format!("{error:#}"));
                cx.notify();
                return;
            }
        };
        self.running = true;
        self.preview = None;
        self.error = None;
        self.status = Some("Reloading column…".into());
        self.cancel = Some(operation.cancel_handle());
        self.update_read_only(cx);
        cx.notify();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { operation.execute() })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.running = false;
                this.cancel = None;
                match result.and_then(|structure| this.accept_structure(structure, window, cx)) {
                    Ok(()) => this.status = None,
                    Err(error) => {
                        this.structure = None;
                        this.status = None;
                        this.error = Some(format!("{error:#}"));
                    }
                }
                this.update_read_only(cx);
                cx.notify();
            })
            .log_err();
        }));
    }
}

impl Focusable for ColumnEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl EventEmitter<ItemEvent> for ColumnEditor {}

impl Item for ColumnEditor {
    type Event = ItemEvent;

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        format!("{}.{} · Column", table_label(&self.table), self.column.name).into()
    }

    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Pencil))
    }

    fn to_item_events(event: &Self::Event, emit: &mut dyn FnMut(ItemEvent)) {
        emit(*event);
    }

    fn is_dirty(&self, cx: &App) -> bool {
        let (name, type_name) = self.draft(cx);
        name != self.column.name || type_name != self.column.type_name
    }

    fn can_save(&self, _: &App) -> bool {
        !self.connection.is_read_only()
            && self
                .structure
                .as_ref()
                .is_some_and(|structure| structure.editability_error.is_none())
    }

    fn save(
        &mut self,
        options: SaveOptions,
        _: Entity<project::Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        // Workspace autosave must not execute schema changes.
        if options.autosave || !self.is_dirty(cx) {
            return Task::ready(Ok(()));
        }
        if self.preview.as_ref() != Some(&self.draft(cx)) {
            return Task::ready(
                self.preview_sql(window, cx)
                    .and_then(|()| Err(anyhow!("Review the SQL preview, then choose Apply"))),
            );
        }
        self.save_changes(window, cx)
    }

    fn reload(
        &mut self,
        _: Entity<project::Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.revert(window, cx);
        Task::ready(Ok(()))
    }
}

impl Render for ColumnEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dirty = self.is_dirty(cx);
        let editable = self.editable();
        let type_editable = self.connection.supports_column_type_changes();
        let reason = self
            .structure
            .as_ref()
            .and_then(|structure| structure.editability_error.clone());
        v_flex()
            .id("database-column-editor")
            .size_full()
            .p_6()
            .gap_3()
            .overflow_y_scroll()
            .child(Label::new(format!("{}.{}", table_label(&self.table), self.column.name)).size(LabelSize::Large))
            .when_some(reason, |view, reason| view.child(Label::new(reason).color(Color::Muted)))
            .child(field("Name", self.name.clone(), cx))
            .child(field("Type", self.type_name.clone(), cx))
            .when(!type_editable, |view| view.child(Label::new("SQLite supports renaming columns here. Changing a column type requires rebuilding the table.").color(Color::Muted)))
            .child(h_flex().gap_3().flex_wrap()
                .child(Label::new(if self.column.nullable { "Nullable" } else { "NOT NULL" }).color(Color::Muted))
                .when(self.column.is_primary_key, |view| view.child(Label::new("Primary key").color(Color::Muted)))
                .when_some(self.column.default_value.clone(), |view, value| view.child(Label::new(format!("Default: {value}")).color(Color::Muted))))
            .child(h_flex().gap_2().flex_wrap()
                .child(Button::new("preview-column-sql", "Preview SQL")
                    .disabled(!editable || !dirty)
                    .on_click(cx.listener(|this, _, window, cx| { this.preview_sql(window, cx).log_err(); })))
                .child(Button::new("apply-column-change", "Apply")
                    .disabled(!editable || !dirty || self.preview.as_ref() != Some(&self.draft(cx)))
                    .on_click(cx.listener(|this, _, window, cx| {
                        let task = this.save_changes(window, cx);
                        this.task = Some(cx.spawn(async move |_, _| { task.await.log_err(); }));
                    })))
                .child(Button::new("revert-column-change", "Revert")
                    .disabled(self.running || !dirty)
                    .on_click(cx.listener(|this, _, window, cx| this.revert(window, cx))))
                .child(Button::new("reload-column", "Reload")
                    .disabled(self.running || dirty)
                    .on_click(cx.listener(|this, _, window, cx| this.reload_column(window, cx)))))
            .when_some(self.status.clone(), |view, status| view.child(Label::new(status).color(Color::Muted)))
            .when_some(self.error.clone(), |view, error| view.child(Label::new(error).color(Color::Error)))
            .when(self.preview.is_some(), |view| view
                .child(Label::new("SQL preview · Apply changes the database schema immediately").color(Color::Muted))
                .child(div().h(px(180.)).flex_shrink_0().border_1().border_color(cx.theme().colors().border).child(self.sql.clone())))
    }
}

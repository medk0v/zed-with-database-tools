mod column_editor;
mod completion;
mod table_editor;

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    rc::Rc,
    sync::Arc,
};

use anyhow::{Context as _, Result};
use database_connection::{
    CancelQuery as QueryCancellation, ColumnMetadata, ConnectionColor, ConnectionConfig,
    DatabaseConnection, DatabaseKind, QueryResult, Secrets, SshConfig, TableInfo, TableStructure,
};
use editor::Editor;
use gpui::{
    Action, App, AppContext, AsyncWindowContext, ClipboardItem, Context, DismissEvent, Entity,
    EventEmitter, FocusHandle, Focusable, Global, IntoElement, MouseButton, Pixels, Point,
    PromptLevel, Render, SharedString, Subscription, Task, WeakEntity, Window, actions, anchored,
    deferred, px, rgb,
};
use language::{Language, LanguageConfig};
use tabular_data_preview::{
    TableView,
    types::{LineNumber, TableCell, TableLikeContent},
};
use ui::{ContextMenu, ContextMenuEntry, Tooltip, prelude::*, table_row::TableRow};
use util::ResultExt;
use workspace::{
    Item, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    database,
    [ToggleFocus, NewConnection, RunQuery, CancelQuery]
);

const ROW_LIMIT: u32 = database_connection::DEFAULT_ROW_LIMIT;

pub(crate) struct ProfilesChanged {
    writes: HashSet<String>,
}
impl EventEmitter<()> for ProfilesChanged {}
pub(crate) struct ProfileAppearance {
    pub(crate) id: String,
    pub(crate) color: Option<ConnectionColor>,
}
impl EventEmitter<ProfileAppearance> for ProfilesChanged {}
struct ProfileEvents(Entity<ProfilesChanged>);
impl Global for ProfileEvents {}

pub(crate) fn profile_events(cx: &App) -> Entity<ProfilesChanged> {
    cx.global::<ProfileEvents>().0.clone()
}

fn begin_profile_write(id: &str, cx: &mut App) -> bool {
    profile_events(cx).update(cx, |events, _| events.writes.insert(id.to_owned()))
}

fn end_profile_write(id: &str, cx: &mut App) {
    profile_events(cx).update(cx, |events, _| {
        events.writes.remove(id);
    });
}

fn profiles_changed(cx: &mut App) {
    let events = cx.global::<ProfileEvents>().0.clone();
    events.update(cx, |_, cx| cx.emit(()));
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TableIdentity {
    pub connection_id: String,
    pub schema: Option<String>,
    pub name: String,
}

impl TableIdentity {
    pub(crate) fn new(connection_id: String, table: &TableInfo) -> Self {
        Self {
            connection_id,
            schema: table.schema.clone(),
            name: table.name.clone(),
        }
    }
}

#[derive(Default)]
pub(crate) struct TableEvents {
    orders: HashMap<TableIdentity, Vec<String>>,
}

pub(crate) struct ColumnOrderChanged {
    pub table: TableIdentity,
    pub columns: Vec<String>,
}

pub(crate) struct TableSchemaChanged {
    pub table: TableIdentity,
    pub renamed: Option<(String, String)>,
    pub structure: Option<TableStructure>,
}

impl EventEmitter<ColumnOrderChanged> for TableEvents {}
impl EventEmitter<TableSchemaChanged> for TableEvents {}
struct TableEventStore(Entity<TableEvents>);
impl Global for TableEventStore {}

pub(crate) fn table_events(cx: &App) -> Entity<TableEvents> {
    cx.global::<TableEventStore>().0.clone()
}

pub(crate) fn column_order(table: &TableIdentity, cx: &App) -> Vec<String> {
    table_events(cx)
        .read(cx)
        .orders
        .get(table)
        .cloned()
        .unwrap_or_default()
}

pub(crate) fn set_column_order(table: TableIdentity, columns: Vec<String>, cx: &mut App) {
    table_events(cx).update(cx, |events, cx| {
        if events.orders.get(&table) != Some(&columns) {
            events.orders.insert(table.clone(), columns.clone());
            cx.emit(ColumnOrderChanged { table, columns });
        }
    });
}

pub(crate) fn table_schema_changed(
    table: TableIdentity,
    renamed: Option<(String, String)>,
    structure: Option<TableStructure>,
    cx: &mut App,
) {
    table_events(cx).update(cx, |events, cx| {
        if let Some((old_name, new_name)) = &renamed {
            if let Some(columns) = events.orders.get_mut(&table) {
                for column in columns {
                    if column == old_name {
                        *column = new_name.clone();
                    }
                }
            }
        }
        cx.emit(TableSchemaChanged {
            table,
            renamed,
            structure,
        });
    });
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum SchemaGroup {
    Columns,
    Keys,
    ForeignKeys,
    Indexes,
    Checks,
}

impl SchemaGroup {
    fn label(self) -> &'static str {
        match self {
            Self::Columns => "columns",
            Self::Keys => "keys",
            Self::ForeignKeys => "foreign keys",
            Self::Indexes => "indexes",
            Self::Checks => "checks",
        }
    }
}

#[derive(Default)]
struct TableTreeState {
    expanded: bool,
    groups: HashSet<SchemaGroup>,
    expanded_columns: HashSet<String>,
    structure: Option<TableStructure>,
    task: Option<Task<()>>,
    error: Option<String>,
}

#[derive(Clone)]
struct DraggedTreeColumn {
    table: TableIdentity,
    column: String,
}

impl Render for DraggedTreeColumn {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px_2()
            .py_1()
            .gap_1()
            .bg(cx.theme().colors().elevated_surface_background)
            .child(Icon::new(IconName::Table).size(IconSize::Small))
            .child(Label::new(self.column.clone()))
    }
}

const CONNECTION_COLORS: [(Option<ConnectionColor>, &str); 7] = [
    (None, "None"),
    (Some(ConnectionColor::Red), "Red"),
    (Some(ConnectionColor::Orange), "Orange"),
    (Some(ConnectionColor::Yellow), "Yellow"),
    (Some(ConnectionColor::Green), "Green"),
    (Some(ConnectionColor::Blue), "Blue"),
    (Some(ConnectionColor::Purple), "Purple"),
];

pub(crate) fn connection_color(color: Option<ConnectionColor>) -> Color {
    let value = match color {
        None => return Color::Muted,
        Some(ConnectionColor::Red) => 0xe57373,
        Some(ConnectionColor::Orange) => 0xffb074,
        Some(ConnectionColor::Yellow) => 0xe5cf76,
        Some(ConnectionColor::Green) => 0x81c995,
        Some(ConnectionColor::Blue) => 0x78a9ff,
        Some(ConnectionColor::Purple) => 0xc69bf2,
    };
    Color::Custom(rgb(value).into())
}

pub fn init(cx: &mut App) {
    let table_events = cx.new(|_| TableEvents::default());
    cx.set_global(TableEventStore(table_events));
    let events = cx.new(|_| ProfilesChanged {
        writes: HashSet::new(),
    });
    cx.set_global(ProfileEvents(events));
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &NewConnection, window, cx| {
            if let Some(panel) = workspace.panel::<DatabasePanel>(cx) {
                window.defer(cx, move |window, cx| {
                    panel.update(cx, |panel, cx| panel.new_connection(window, cx));
                });
            }
        });
    })
    .detach();
}

struct ConnectionEntry {
    config: ConnectionConfig,
    connection: Option<DatabaseConnection>,
    tables: Vec<TableInfo>,
    expanded: bool,
    connecting: bool,
    task: Option<Task<()>>,
}

#[derive(Clone)]
enum PanelCommand {
    Connect,
    Disconnect,
    Edit,
    Console(Option<TableInfo>),
    OpenTable(TableInfo),
    Refresh,
    Remove,
    Color(Option<ConnectionColor>),
    Scope(Option<Vec<PathBuf>>),
    DropTable(TableInfo),
}

pub struct DatabasePanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    entries: Vec<ConnectionEntry>,
    table_states: HashMap<TableIdentity, TableTreeState>,
    error: Option<String>,
    position: DockPosition,
    _load_task: Option<Task<()>>,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    _subscriptions: Vec<Subscription>,
}

impl DatabasePanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        cx.update(|_, cx| cx.new(|cx| Self::new(workspace, cx)))
    }

    fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let events = cx.global::<ProfileEvents>().0.clone();
        let mut subscriptions = vec![cx.subscribe(&events, |this, _, _: &(), cx| {
            this.reload_profiles(cx);
        })];
        let table_events = table_events(cx);
        subscriptions.push(
            cx.subscribe(&table_events, |_, _, _: &ColumnOrderChanged, cx| {
                cx.notify()
            }),
        );
        subscriptions.push(cx.subscribe(
            &table_events,
            |this, _, event: &TableSchemaChanged, cx| {
                if let Some(state) = this.table_states.get_mut(&event.table) {
                    if let Some((old_name, new_name)) = &event.renamed {
                        if state.expanded_columns.remove(old_name) {
                            state.expanded_columns.insert(new_name.clone());
                        }
                    }
                    state.structure = event.structure.clone();
                    state.task = None;
                    state.error = None;
                    if state.expanded && state.structure.is_none() {
                        this.load_table_structure(&event.table, cx);
                    }
                }
                cx.notify();
            },
        ));
        if let Some(workspace) = workspace.upgrade() {
            let project = workspace.read(cx).project().clone();
            subscriptions.push(cx.observe(&project, |_, _, cx| cx.notify()));
        }
        let mut panel = Self {
            workspace,
            focus_handle: cx.focus_handle(),
            entries: Vec::new(),
            table_states: HashMap::new(),
            error: None,
            position: DockPosition::Right,
            _load_task: None,
            context_menu: None,
            _subscriptions: subscriptions,
        };
        panel.reload_profiles(cx);
        panel
    }

    fn project_roots(&self, cx: &App) -> Vec<PathBuf> {
        self.workspace
            .read_with(cx, |workspace, cx| {
                if !workspace.project().read(cx).is_local() {
                    return Vec::new();
                }
                let roots: Vec<_> = workspace
                    .visible_worktrees(cx)
                    .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                    .collect();
                if roots.is_empty() {
                    return roots;
                }
                database_connection::normalize_project_roots(&roots)
                    .log_err()
                    .unwrap_or_default()
            })
            .log_err()
            .unwrap_or_default()
    }

    fn reload_profiles(&mut self, cx: &mut Context<Self>) {
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async { load_connections() }).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(configs) => {
                        for config in &configs {
                            profile_events(cx).update(cx, |_, cx| {
                                cx.emit(ProfileAppearance {
                                    id: config.id.clone(),
                                    color: config.color,
                                })
                            });
                        }
                        for index in (0..this.entries.len()).rev() {
                            let existing = &this.entries[index].config;
                            let replacement =
                                configs.iter().find(|config| config.id == existing.id);
                            match replacement {
                                None => {
                                    this.disconnect(index, cx);
                                    this.entries.remove(index);
                                }
                                Some(config) => {
                                    let mut transport = config.clone();
                                    transport.color = existing.color;
                                    transport.project_scope = existing.project_scope.clone();
                                    transport.name = existing.name.clone();
                                    if &transport != existing {
                                        this.disconnect(index, cx);
                                    }
                                    this.entries[index].config = config.clone();
                                }
                            }
                        }
                        for config in configs {
                            if !this
                                .entries
                                .iter()
                                .any(|entry| entry.config.id == config.id)
                            {
                                this.entries.push(Self::entry(config));
                            }
                        }
                    }
                    Err(error) => {
                        this.error = Some(format!("Could not load connections: {error:#}"))
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn entry(config: ConnectionConfig) -> ConnectionEntry {
        ConnectionEntry {
            config,
            connection: None,
            tables: Vec::new(),
            expanded: false,
            connecting: false,
            task: None,
        }
    }

    fn new_connection(&self, window: &mut Window, cx: &mut Context<Self>) {
        let panel = cx.entity().downgrade();
        let roots = self.project_roots(cx);
        let config = ConnectionConfig {
            project_scope: (!roots.is_empty()).then(|| roots.clone()),
            ..ConnectionConfig::default()
        };
        let item = cx.new(|cx| ConnectionEditor::new(panel, config, roots, true, window, cx));
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
            })
            .log_err();
    }

    fn edit_connection(&self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        let config = entry.config.clone();
        let panel = cx.entity().downgrade();
        let roots = self.project_roots(cx);
        let item = cx.new(|cx| ConnectionEditor::new(panel, config, roots, false, window, cx));
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
            })
            .log_err();
    }

    fn save_connection(
        &mut self,
        config: ConnectionConfig,
        expected: Option<ConnectionConfig>,
        secrets: Secrets,
        form: WeakEntity<ConnectionEditor>,
        cx: &mut Context<Self>,
    ) {
        self.error = None;
        cx.spawn(async move |this, cx| {
            let stored_config = config.clone();
            let id = config.id.clone();
            let acquired = cx.update(|cx| begin_profile_write(&id, cx));
            let mut persisted = false;
            let result: Result<()> = async {
                anyhow::ensure!(acquired, "This connection is already being saved or removed in another window. Try again when it finishes.");
                cx.background_spawn(async move {
                    database_connection::save_profile_if_unchanged(&profiles_path(), &stored_config, expected.as_ref())
                }).await?;
                persisted = true;
                for (kind, username, secret) in [
                    ("database", config.user.as_str(), secrets.password.as_deref()),
                    ("ssh", "ssh", secrets.ssh_secret.as_deref()),
                ] {
                    if let Some(secret) = secret {
                        cx.update(|cx| cx.write_credentials(&credential_key(&config.id, kind), username, secret.as_bytes()))
                            .await.with_context(|| format!("Connection settings saved, but could not save {kind} password to Keychain; retry saving"))?;
                    }
                }
                Ok(())
            }.await;
            let error = result.as_ref().err().map(|error| format!("{error:#}"));
            form.update(cx, |form, cx| {
                form.saving = false;
                form.error = error;
                if persisted { form.original = Some(config.clone()); }
                cx.notify();
            }).log_err();
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        let index = if let Some(index) = this.entries.iter().position(|entry| entry.config.id == config.id) {
                            this.disconnect(index, cx);
                            this.entries[index] = Self::entry(config);
                            index
                        } else {
                            this.entries.push(Self::entry(config));
                            this.entries.len() - 1
                        };
                        this.connect(index, cx);
                    }
                    Err(error) => this.error = Some(format!("Could not save connection: {error:#}")),
                }
                cx.notify();
            }).log_err();
            if persisted { cx.update(profiles_changed); }
            if acquired { cx.update(|cx| end_profile_write(&id, cx)); }
        }).detach();
        cx.notify();
    }

    fn connect(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get_mut(index) else {
            return;
        };
        if entry.connecting {
            return;
        }
        if entry.connection.is_some() {
            entry.expanded = !entry.expanded;
            cx.notify();
            return;
        }
        entry.connecting = true;
        let config = entry.config.clone();
        let connection_id = config.id.clone();
        let password_task = cx.read_credentials(&credential_key(&config.id, "database"));
        let ssh_task = cx.read_credentials(&credential_key(&config.id, "ssh"));
        self.error = None;
        entry.task = Some(cx.spawn(async move |this, cx| {
            let result: Result<(DatabaseConnection, Vec<TableInfo>)> = async {
                let secrets = Secrets {
                    password: password_task
                        .await?
                        .map(|(_, bytes)| String::from_utf8(bytes))
                        .transpose()?,
                    ssh_secret: ssh_task
                        .await?
                        .map(|(_, bytes)| String::from_utf8(bytes))
                        .transpose()?,
                };
                cx.background_spawn(async move {
                    let connection = DatabaseConnection::connect(&config, secrets)?;
                    let tables = connection.list_tables()?;
                    Ok((connection, tables))
                })
                .await
            }
            .await;
            this.update(cx, |this, cx| {
                if let Some(entry) = this
                    .entries
                    .iter_mut()
                    .find(|entry| entry.config.id == connection_id)
                {
                    entry.connecting = false;
                    match result {
                        Ok((connection, tables)) => {
                            entry.connection = Some(connection);
                            entry.tables = tables;
                            entry.expanded = true;
                        }
                        Err(error) => this.error = Some(format!("Connection failed: {error:#}")),
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn disconnect(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get_mut(index) else {
            return;
        };
        self.table_states
            .retain(|identity, _| identity.connection_id != entry.config.id);
        entry.task = None;
        entry.connecting = false;
        entry.tables.clear();
        entry.expanded = false;
        let Some(connection) = entry.connection.take() else {
            cx.notify();
            return;
        };
        connection.mark_disconnected();
        // Reconnecting or editing the profile must not cancel queued teardown.
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { connection.disconnect() })
                .await;
            this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.error = Some(format!("Disconnect failed: {error:#}"));
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
        cx.notify();
    }

    fn open_console(
        &self,
        index: usize,
        table: Option<TableInfo>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        let Some(connection) = entry.connection.clone() else {
            return;
        };
        let title = match &table {
            Some(table) => format!("{} · {}", table.name, entry.config.name),
            None => format!("Console · {}", entry.config.name),
        };
        if let Some(table) = table {
            let item = cx.new(|cx| {
                table_editor::DatabaseTableEditor::new(
                    connection,
                    title,
                    table,
                    entry.config.color,
                    entry.config.id.clone(),
                    window,
                    cx,
                )
            });
            item.update(cx, |item, cx| item.refresh(cx));
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
                })
                .log_err();
            return;
        }
        self.open_sql_console(index, None, window, cx);
    }

    fn open_sql_console(
        &self,
        index: usize,
        table: Option<TableInfo>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        let Some(connection) = entry.connection.clone() else {
            return;
        };
        let title = format!("Console · {}", entry.config.name);
        let query = table
            .map(|table| connection.browse_sql(&table, ROW_LIMIT, 0))
            .unwrap_or_else(|| "SELECT 1;".to_owned());
        let identifiers = entry
            .tables
            .iter()
            .flat_map(|table| {
                let mut names = vec![table.name.clone()];
                if let Some(schema) = &table.schema {
                    names.push(format!("{schema}.{}", table.name));
                }
                names
            })
            .collect();
        let console = cx.new(|cx| {
            DatabaseConsole::new(
                connection,
                title,
                query,
                identifiers,
                entry.config.color,
                entry.config.id.clone(),
                window,
                cx,
            )
        });
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.add_item_to_active_pane(Box::new(console), None, true, window, cx);
            })
            .log_err();
    }
    fn perform(
        &mut self,
        id: &str,
        command: PanelCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.entries.iter().position(|entry| entry.config.id == id) else {
            return;
        };
        match command {
            PanelCommand::Connect => self.connect(index, cx),
            PanelCommand::Disconnect => self.disconnect(index, cx),
            PanelCommand::Edit => self.edit_connection(index, window, cx),
            PanelCommand::Console(table) => self.open_sql_console(index, table, window, cx),
            PanelCommand::OpenTable(table) => self.open_console(index, Some(table), window, cx),
            PanelCommand::Refresh => self.refresh_tables(index, cx),
            PanelCommand::Remove => self.remove_connection(index, window, cx),
            PanelCommand::Color(color) => {
                let id = id.to_owned();
                self.update_metadata(
                    move || database_connection::update_profile_color(&profiles_path(), &id, color),
                    cx,
                );
            }
            PanelCommand::Scope(scope) => {
                let id = id.to_owned();
                self.update_metadata(
                    move || database_connection::update_profile_scope(&profiles_path(), &id, scope),
                    cx,
                );
            }
            PanelCommand::DropTable(table) => self.drop_table(index, table, window, cx),
        }
    }

    fn refresh_tables(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get_mut(index) else {
            return;
        };
        let Some(connection) = entry.connection.clone() else {
            return;
        };
        let id = entry.config.id.clone();
        entry.task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { connection.list_tables() })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(tables) => {
                        if let Some(entry) =
                            this.entries.iter_mut().find(|entry| entry.config.id == id)
                        {
                            let identities: HashSet<_> = tables
                                .iter()
                                .map(|table| TableIdentity::new(id.clone(), table))
                                .collect();
                            entry.tables = tables;
                            entry.expanded = true;
                            this.table_states.retain(|identity, _| {
                                identity.connection_id != id || identities.contains(identity)
                            });
                            let mut reload = Vec::new();
                            for (identity, state) in &mut this.table_states {
                                if identity.connection_id == id {
                                    state.structure = None;
                                    state.task = None;
                                    if state.expanded {
                                        reload.push(identity.clone());
                                    }
                                }
                            }
                            for identity in reload {
                                this.load_table_structure(&identity, cx);
                            }
                        }
                    }
                    Err(error) => this.error = Some(format!("Could not refresh tables: {error:#}")),
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn update_metadata(
        &mut self,
        update: impl FnOnce() -> Result<()> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        self.error = None;
        cx.spawn(
            async move |this, cx| match cx.background_spawn(async move { update() }).await {
                Ok(()) => cx.update(profiles_changed),
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.error = Some(format!("Could not update connection: {error:#}"));
                        cx.notify();
                    })
                    .log_err();
                }
            },
        )
        .detach();
    }

    fn remove_connection(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        let id = entry.config.id.clone();
        let prompt = window.prompt(
            PromptLevel::Warning,
            &format!("Remove connection “{}” from the list?", entry.config.name),
            Some("The saved connection and its saved passwords will be removed. The database and its tables will not be deleted."),
            &["Remove connection", "Cancel"], cx,
        );
        cx.spawn(async move |this, cx| {
            if prompt.await? != 0 { return Ok(()); }
            let acquired = cx.update(|cx| begin_profile_write(&id, cx));
            let result: Result<()> = async {
                anyhow::ensure!(acquired, "This connection is already being saved or removed in another window. Try again when it finishes.");
                let stored_id = id.clone();
                cx.background_spawn(async move {
                    database_connection::remove_profile(&profiles_path(), &stored_id)
                }).await?;
                this.update(cx, |this, cx| {
                    if let Some(index) = this.entries.iter().position(|entry| entry.config.id == id) {
                        this.disconnect(index, cx);
                        this.entries.remove(index);
                    }
                    cx.notify();
                }).log_err();
                cx.update(profiles_changed);
                for kind in ["database", "ssh"] {
                    let key = credential_key(&id, kind);
                    if cx.update(|cx| cx.read_credentials(&key)).await?.is_some() {
                        cx.update(|cx| cx.delete_credentials(&key)).await
                            .with_context(|| format!("Connection removed, but its {kind} password could not be removed from Keychain"))?;
                    }
                }
                Ok(())
            }.await;
            if acquired { cx.update(|cx| end_profile_write(&id, cx)); }
            if let Err(error) = result {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("Could not finish removing connection: {error:#}"));
                    cx.notify();
                }).log_err();
            }
            anyhow::Ok(())
        }).detach_and_log_err(cx);
    }

    fn drop_table(
        &mut self,
        index: usize,
        table: TableInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        let Some(connection) = entry.connection.clone() else {
            return;
        };
        if connection.is_read_only() {
            return;
        }
        let id = entry.config.id.clone();
        let object = if table.is_view { "view" } else { "table" };
        let prompt = window.prompt(
            PromptLevel::Warning,
            &format!("Delete {object} “{}” from “{}”?", table_label(&table), entry.config.name),
            Some(if table.is_view {
                "This runs DROP VIEW on the connected database. Dependent objects are not removed automatically."
            } else {
                "This runs DROP TABLE and permanently deletes all rows in this table. Dependent objects are not removed automatically."
            }),
            &["Delete", "Cancel"], cx,
        );
        cx.spawn(async move |this, cx| {
            if prompt.await? != 0 {
                return Ok(());
            }
            let result = cx
                .background_spawn(async move { connection.start_drop_table(&table)?.execute() })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(_) => {
                        if let Some(index) =
                            this.entries.iter().position(|entry| entry.config.id == id)
                        {
                            this.refresh_tables(index, cx);
                        }
                    }
                    Err(error) => {
                        this.error = Some(format!("Could not delete {object}: {error:#}"))
                    }
                }
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn toggle_table(&mut self, identity: &TableIdentity, cx: &mut Context<Self>) {
        let state = self.table_states.entry(identity.clone()).or_default();
        state.expanded = !state.expanded;
        if state.expanded && state.structure.is_none() && state.task.is_none() {
            self.load_table_structure(identity, cx);
        }
        cx.notify();
    }

    fn load_table_structure(&mut self, identity: &TableIdentity, cx: &mut Context<Self>) {
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.config.id == identity.connection_id)
        else {
            return;
        };
        let Some(connection) = entry.connection.clone() else {
            return;
        };
        let Some(table) = entry
            .tables
            .iter()
            .find(|table| table.schema == identity.schema && table.name == identity.name)
            .cloned()
        else {
            return;
        };
        let state = self.table_states.entry(identity.clone()).or_default();
        state.error = None;
        let identity = identity.clone();
        state.task = Some(cx.spawn(async move |this, cx| {
            let operation = loop {
                if connection.is_busy() {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(50))
                        .await;
                    continue;
                }
                break connection.start_table_structure(&table);
            };
            let result = match operation {
                Ok(operation) => {
                    cx.background_spawn(async move { operation.execute() })
                        .await
                }
                Err(error) => Err(error),
            };
            this.update(cx, |this, cx| {
                if let Some(state) = this.table_states.get_mut(&identity) {
                    state.task = None;
                    match result {
                        Ok(structure) => state.structure = Some(structure),
                        Err(error) => {
                            state.error = Some(format!("Could not load structure: {error:#}"))
                        }
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn open_column_editor(
        &self,
        identity: &TableIdentity,
        column: &ColumnMetadata,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.config.id == identity.connection_id)
        else {
            return;
        };
        let Some(connection) = entry.connection.clone() else {
            return;
        };
        let Some(structure) = self
            .table_states
            .get(identity)
            .and_then(|state| state.structure.clone())
        else {
            return;
        };
        let item = cx.new(|cx| {
            column_editor::ColumnEditor::new(
                identity.connection_id.clone(),
                connection,
                structure,
                column.clone(),
                window,
                cx,
            )
        });
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
            })
            .log_err();
    }

    fn move_tree_column(
        &mut self,
        dragged: &DraggedTreeColumn,
        identity: &TableIdentity,
        target: &str,
        cx: &mut Context<Self>,
    ) {
        if &dragged.table != identity {
            return;
        }
        let Some(structure) = self
            .table_states
            .get(identity)
            .and_then(|state| state.structure.as_ref())
        else {
            return;
        };
        let mut names = ordered_columns(structure, &column_order(identity, cx))
            .into_iter()
            .map(|column| column.name.clone())
            .collect::<Vec<_>>();
        let Some(from) = names.iter().position(|name| name == &dragged.column) else {
            return;
        };
        let Some(to) = names.iter().position(|name| name == target) else {
            return;
        };
        if from == to {
            return;
        }
        let column = names.remove(from);
        names.insert(to, column);
        set_column_order(identity.clone(), names, cx);
        cx.notify();
    }

    fn render_table_tree(
        &self,
        index: usize,
        table: &TableInfo,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let entry = &self.entries[index];
        let identity = TableIdentity::new(entry.config.id.clone(), table);
        let element_id = format!("table-{index}-{}", table_label(table));
        let state = self.table_states.get(&identity);
        let expanded = state.is_some_and(|state| state.expanded);
        v_flex()
            .min_w_0()
            .child(
                h_flex()
                    .id(SharedString::from(element_id.clone()))
                    .pl_5()
                    .pr_2()
                    .gap_1()
                    .min_w_0()
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener({
                            let id = entry.config.id.clone();
                            let table = table.clone();
                            move |_, event: &gpui::MouseDownEvent, window, cx| {
                                cx.stop_propagation();
                                let (id, table, position) =
                                    (id.clone(), table.clone(), event.position);
                                cx.defer_in(window, move |this, window, cx| {
                                    this.show_menu(&id, Some(table), position, window, cx)
                                });
                            }
                        }),
                    )
                    .child(
                        IconButton::new(
                            SharedString::from(format!("{element_id}-toggle")),
                            if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            },
                        )
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text(if expanded {
                            "Collapse table"
                        } else {
                            "Expand table"
                        }))
                        .on_click(cx.listener({
                            let identity = identity.clone();
                            move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.toggle_table(&identity, cx);
                            }
                        })),
                    )
                    .child(
                        h_flex()
                            .id(SharedString::from(format!("{element_id}-open")))
                            .py_0p5()
                            .gap_1()
                            .flex_1()
                            .min_w_0()
                            .cursor_pointer()
                            .on_click(cx.listener({
                                let table = table.clone();
                                move |this, _, window, cx| {
                                    this.open_console(index, Some(table.clone()), window, cx)
                                }
                            }))
                            .child(
                                Icon::new(IconName::Table)
                                    .size(IconSize::Small)
                                    .color(connection_color(entry.config.color)),
                            )
                            .child(Label::new(table_label(table)).truncate()),
                    ),
            )
            .when(expanded, |view| {
                let Some(state) = state else {
                    return view;
                };
                if state.task.is_some() {
                    return view.child(
                        div()
                            .pl_10()
                            .py_1()
                            .child(Label::new("Loading structure…").color(Color::Muted)),
                    );
                }
                if let Some(error) = &state.error {
                    return view.child(
                        v_flex()
                            .pl_10()
                            .gap_1()
                            .child(Label::new(error.clone()).color(Color::Error))
                            .child(
                                Button::new(
                                    SharedString::from(format!("{element_id}-retry")),
                                    "Retry",
                                )
                                .on_click(cx.listener({
                                    let identity = identity.clone();
                                    move |this, _, _, cx| this.load_table_structure(&identity, cx)
                                })),
                            ),
                    );
                }
                let Some(structure) = &state.structure else {
                    return view;
                };
                view.children(
                    [
                        SchemaGroup::Columns,
                        SchemaGroup::Keys,
                        SchemaGroup::ForeignKeys,
                        SchemaGroup::Indexes,
                        SchemaGroup::Checks,
                    ]
                    .into_iter()
                    .map(|group| {
                        self.render_schema_group(
                            &identity,
                            &element_id,
                            state,
                            structure,
                            group,
                            cx,
                        )
                    }),
                )
            })
            .into_any_element()
    }

    fn render_schema_group(
        &self,
        identity: &TableIdentity,
        table_id: &str,
        state: &TableTreeState,
        structure: &TableStructure,
        group: SchemaGroup,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let entries = match group {
            SchemaGroup::Columns => &[][..],
            SchemaGroup::Keys => structure.keys.as_slice(),
            SchemaGroup::ForeignKeys => structure.foreign_keys.as_slice(),
            SchemaGroup::Indexes => structure.indexes.as_slice(),
            SchemaGroup::Checks => structure.checks.as_slice(),
        };
        let count = if group == SchemaGroup::Columns {
            structure.columns.len()
        } else {
            entries.len()
        };
        let expanded = state.groups.contains(&group);
        let group_id = format!("{table_id}-{}", group.label());
        v_flex()
            .min_w_0()
            .child(
                h_flex()
                    .pl_10()
                    .pr_2()
                    .gap_1()
                    .min_w_0()
                    .child(
                        IconButton::new(
                            SharedString::from(format!("{group_id}-toggle")),
                            if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            },
                        )
                        .icon_size(IconSize::Small)
                        .on_click(cx.listener({
                            let identity = identity.clone();
                            move |this, _, _, cx| {
                                cx.stop_propagation();
                                if let Some(state) = this.table_states.get_mut(&identity) {
                                    if !state.groups.remove(&group) {
                                        state.groups.insert(group);
                                    }
                                }
                                cx.notify();
                            }
                        })),
                    )
                    .child(
                        Icon::new(if expanded {
                            IconName::FolderOpen
                        } else {
                            IconName::Folder
                        })
                        .size(IconSize::Small)
                        .color(Color::Accent),
                    )
                    .child(Label::new(group.label()))
                    .child(Label::new(count.to_string()).color(Color::Muted)),
            )
            .when(expanded, |view| {
                if group == SchemaGroup::Columns {
                    view.children(
                        ordered_columns(structure, &column_order(identity, cx))
                            .into_iter()
                            .map(|column| {
                                self.render_tree_column(identity, &group_id, state, column, cx)
                            }),
                    )
                } else {
                    let icon = match group {
                        SchemaGroup::Keys => IconName::Lock,
                        SchemaGroup::ForeignKeys => IconName::Link,
                        SchemaGroup::Checks => IconName::Check,
                        _ => IconName::ListTree,
                    };
                    view.children(entries.iter().map(|entry| {
                        h_flex()
                            .pl_16()
                            .pr_2()
                            .py_0p5()
                            .gap_2()
                            .min_w_0()
                            .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                            .child(Label::new(entry.name.clone()))
                            .child(
                                Label::new(entry.detail.clone())
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                    }))
                }
            })
            .into_any_element()
    }

    fn render_tree_column(
        &self,
        identity: &TableIdentity,
        group_id: &str,
        state: &TableTreeState,
        column: &ColumnMetadata,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let element_id = format!("{group_id}-{}", column.name);
        let expanded = state.expanded_columns.contains(&column.name);
        let dragged = DraggedTreeColumn {
            table: identity.clone(),
            column: column.name.clone(),
        };
        let editable = state
            .structure
            .as_ref()
            .is_some_and(|structure| structure.editability_error.is_none());
        v_flex()
            .min_w_0()
            .child(
                h_flex()
                    .id(SharedString::from(element_id.clone()))
                    .pl_16()
                    .pr_2()
                    .gap_1()
                    .min_w_0()
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .drag_over::<DraggedTreeColumn>({
                        let identity = identity.clone();
                        move |style, dragged, _, cx| {
                            if dragged.table == identity {
                                style.bg(cx.theme().colors().drop_target_background)
                            } else {
                                style
                            }
                        }
                    })
                    .on_drop(cx.listener({
                        let identity = identity.clone();
                        let name = column.name.clone();
                        move |this, dragged: &DraggedTreeColumn, _, cx| {
                            this.move_tree_column(dragged, &identity, &name, cx);
                        }
                    }))
                    .child(
                        IconButton::new(
                            SharedString::from(format!("{element_id}-toggle")),
                            if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            },
                        )
                        .icon_size(IconSize::Small)
                        .on_click(cx.listener({
                            let identity = identity.clone();
                            let name = column.name.clone();
                            move |this, _, _, cx| {
                                cx.stop_propagation();
                                if let Some(state) = this.table_states.get_mut(&identity) {
                                    if !state.expanded_columns.remove(&name) {
                                        state.expanded_columns.insert(name.clone());
                                    }
                                }
                                cx.notify();
                            }
                        })),
                    )
                    .child(
                        h_flex()
                            .id(SharedString::from(format!("{element_id}-name")))
                            .gap_2()
                            .py_0p5()
                            .flex_1()
                            .min_w_0()
                            .cursor_pointer()
                            .on_drag(dragged, |dragged, _, _, cx| cx.new(|_| dragged.clone()))
                            .on_click(cx.listener({
                                let identity = identity.clone();
                                let column = column.clone();
                                move |this, _, window, cx| {
                                    this.open_column_editor(&identity, &column, window, cx)
                                }
                            }))
                            .child(
                                Icon::new(if column.is_primary_key {
                                    IconName::Lock
                                } else {
                                    IconName::Table
                                })
                                .size(IconSize::Small)
                                .color(if column.is_primary_key {
                                    Color::Warning
                                } else {
                                    Color::Muted
                                }),
                            )
                            .child(Label::new(column.name.clone()))
                            .child(
                                Label::new(column.type_name.clone())
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                    )
                    .when(editable, |view| {
                        view.child(
                            IconButton::new(
                                SharedString::from(format!("{element_id}-edit")),
                                IconName::Pencil,
                            )
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Edit column"))
                            .on_click(cx.listener({
                                let identity = identity.clone();
                                let column = column.clone();
                                move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.open_column_editor(&identity, &column, window, cx);
                                }
                            })),
                        )
                    }),
            )
            .when(expanded, |view| {
                view.child(
                    v_flex()
                        .pl_20()
                        .pr_2()
                        .py_1()
                        .gap_1()
                        .child(
                            Label::new(format!("Type: {}", column.type_name)).color(Color::Muted),
                        )
                        .child(
                            Label::new(if column.nullable {
                                "Nullable: yes"
                            } else {
                                "Nullable: no"
                            })
                            .color(Color::Muted),
                        )
                        .child(
                            Label::new(format!(
                                "Default: {}",
                                column.default_value.as_deref().unwrap_or("none")
                            ))
                            .color(Color::Muted),
                        )
                        .when(column.is_primary_key, |view| {
                            view.child(Label::new("Primary key").color(Color::Muted))
                        }),
                )
            })
            .into_any_element()
    }

    fn show_menu(
        &mut self,
        id: &str,
        table: Option<TableInfo>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.entries.iter().find(|entry| entry.config.id == id) else {
            return;
        };
        let id = entry.config.id.clone();
        let panel = cx.entity().downgrade();
        let connected = entry.connection.is_some();
        let roots = self.project_roots(cx);
        let menu = ContextMenu::build(window, cx, |menu, _, _| {
            if let Some(table) = table {
                let label = table_label(&table);
                let object = if table.is_view { "view" } else { "table" };
                return menu
                    .item(panel_menu_entry(
                        &panel,
                        &id,
                        "Open data",
                        PanelCommand::OpenTable(table.clone()),
                    ))
                    .item(panel_menu_entry(
                        &panel,
                        &id,
                        "SELECT in SQL console",
                        PanelCommand::Console(Some(table.clone())),
                    ))
                    .entry("Copy table name", None, move |_, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(label.clone()));
                    })
                    .item(panel_menu_entry(
                        &panel,
                        &id,
                        "Refresh tables",
                        PanelCommand::Refresh,
                    ))
                    .separator()
                    .item(
                        panel_menu_entry(
                            &panel,
                            &id,
                            format!("Delete {object}…"),
                            PanelCommand::DropTable(table),
                        )
                        .disabled(entry.config.read_only),
                    );
            }
            menu.item(panel_menu_entry(
                &panel,
                &id,
                if connected || entry.connecting {
                    "Disconnect"
                } else {
                    "Connect"
                },
                if connected || entry.connecting {
                    PanelCommand::Disconnect
                } else {
                    PanelCommand::Connect
                },
            ))
            .item(panel_menu_entry(
                &panel,
                &id,
                "Edit connection…",
                PanelCommand::Edit,
            ))
            .item(
                panel_menu_entry(&panel, &id, "New SQL console", PanelCommand::Console(None))
                    .disabled(!connected),
            )
            .item(
                panel_menu_entry(&panel, &id, "Refresh tables", PanelCommand::Refresh)
                    .disabled(!connected),
            )
            .separator()
            .submenu("Color", {
                let panel = panel.clone();
                let id = id.clone();
                let selected = entry.config.color;
                move |mut menu, _, _| {
                    for (color, label) in CONNECTION_COLORS {
                        menu = menu.item(
                            panel_menu_entry(&panel, &id, label, PanelCommand::Color(color))
                                .toggleable(ui::IconPosition::Start, selected == color)
                                .icon(IconName::Circle)
                                .icon_color(connection_color(color)),
                        );
                    }
                    menu
                }
            })
            .submenu("Visible in", {
                let panel = panel.clone();
                let id = id.clone();
                let global = entry.config.project_scope.is_none();
                move |menu, _, _| {
                    menu.item(
                        panel_menu_entry(&panel, &id, "All projects", PanelCommand::Scope(None))
                            .toggleable(ui::IconPosition::Start, global),
                    )
                    .item(
                        panel_menu_entry(
                            &panel,
                            &id,
                            "This project",
                            PanelCommand::Scope(Some(roots.clone())),
                        )
                        .toggleable(ui::IconPosition::Start, !global)
                        .disabled(roots.is_empty()),
                    )
                }
            })
            .separator()
            .item(panel_menu_entry(
                &panel,
                &id,
                "Remove connection from list…",
                PanelCommand::Remove,
            ))
        });
        window.focus(&menu.focus_handle(cx), cx);
        let subscription =
            cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, window, cx| {
                if this
                    .context_menu
                    .as_ref()
                    .is_some_and(|(menu, _, _)| menu.focus_handle(cx).contains_focused(window, cx))
                {
                    window.focus(&this.focus_handle, cx);
                }
                this.context_menu = None;
                cx.notify();
            });
        self.context_menu = Some((menu, position, subscription));
        cx.notify();
    }
}

fn ordered_columns<'a>(structure: &'a TableStructure, order: &[String]) -> Vec<&'a ColumnMetadata> {
    let mut columns = Vec::with_capacity(structure.columns.len());
    let mut seen = HashSet::new();
    for name in order {
        if seen.insert(name.as_str()) {
            if let Some(column) = structure.columns.iter().find(|column| &column.name == name) {
                columns.push(column);
            }
        }
    }
    columns.extend(
        structure
            .columns
            .iter()
            .filter(|column| !seen.contains(column.name.as_str())),
    );
    columns
}

fn panel_menu_entry(
    panel: &WeakEntity<DatabasePanel>,
    id: &str,
    label: impl Into<SharedString>,
    command: PanelCommand,
) -> ContextMenuEntry {
    let panel = panel.clone();
    let id = id.to_owned();
    ContextMenuEntry::new(label).handler(move |window, cx| {
        panel
            .update(cx, |panel, cx| {
                panel.perform(&id, command.clone(), window, cx)
            })
            .log_err();
    })
}

fn table_label(table: &TableInfo) -> String {
    table
        .schema
        .as_ref()
        .map(|schema| format!("{schema}.{}", table.name))
        .unwrap_or_else(|| table.name.clone())
}

fn profiles_path() -> PathBuf {
    paths::config_dir().join("database-connections.json")
}

impl Focusable for DatabasePanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
impl EventEmitter<PanelEvent> for DatabasePanel {}
impl Panel for DatabasePanel {
    fn persistent_name() -> &'static str {
        "DatabasePanel"
    }
    fn panel_key() -> &'static str {
        "DatabasePanel"
    }
    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }
    fn position_is_valid(&self, position: DockPosition) -> bool {
        position != DockPosition::Bottom
    }
    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }
    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(300.)
    }
    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::Table)
    }
    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Databases")
    }
    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }
    fn activation_priority(&self) -> u32 {
        5
    }
}
impl Render for DatabasePanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let roots = self.project_roots(cx);
        let visible: Vec<_> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.config.visible_in_project(&roots))
            .collect();
        v_flex().id("database-panel").track_focus(&self.focus_handle).size_full()
            .child(h_flex().p_2().justify_between().child(Label::new("Databases"))
                .child(IconButton::new("new-connection", IconName::Plus)
                    .on_click(cx.listener(|this, _, window, cx| this.new_connection(window, cx)))))
            .when_some(self.error.clone(), |view, error| {
                view.child(div().p_2().child(Label::new(error).color(Color::Error)))
            })
            .child(v_flex().id("database-tree").flex_1().min_h_0().overflow_y_scroll()
                .when(visible.is_empty(), |view| {
                    view.child(div().p_3().child(Label::new("No connections for this project. Add a PostgreSQL, MySQL or SQLite connection.").color(Color::Muted)))
                })
                .children(visible.into_iter().map(|(index, entry)| {
                    let color = connection_color(entry.config.color);
                    v_flex().min_w_0()
                        .child(h_flex().id(("connection", index)).px_1().gap_1().min_w_0()
                            .on_mouse_down(MouseButton::Right, cx.listener({ let id = entry.config.id.clone(); move |_, event: &gpui::MouseDownEvent, window, cx| {
                                cx.stop_propagation();
                                let position = event.position;
                                let id = id.clone();
                                cx.defer_in(window, move |this, window, cx| this.show_menu(&id, None, position, window, cx));
                            }}))
                            .child(h_flex().id(("connection-name", index)).flex_1().min_w_0().gap_1().py_1().cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| this.connect(index, cx)))
                                .child(Icon::new(if entry.expanded { IconName::ChevronDown } else { IconName::ChevronRight }).size(IconSize::Small))
                                .child(Icon::new(IconName::Circle).size(IconSize::Small).color(color))
                                .child(div().flex_1().min_w_0().child(Label::new(if entry.connecting {
                                    format!("{} · connecting…", entry.config.name)
                                } else { entry.config.name.clone() }).truncate().color(color)))
                                .when(entry.config.project_scope.is_some(), |view| view.child(Icon::new(IconName::Folder).size(IconSize::Small).color(Color::Muted))))
                            .child(IconButton::new(("edit-connection", index), IconName::Pencil)
                                .on_click(cx.listener(move |this, _, window, cx| this.edit_connection(index, window, cx))))
                            .when(entry.connection.is_some(), |view| view
                                .child(IconButton::new(("console", index), IconName::Terminal)
                                    .on_click(cx.listener(move |this, _, window, cx| this.open_console(index, None, window, cx))))
                                .child(IconButton::new(("disconnect", index), IconName::Close)
                                    .on_click(cx.listener(move |this, _, _, cx| this.disconnect(index, cx))))))
                        .when(entry.expanded, |view| view.children(entry.tables.iter().map(|table| {
                            self.render_table_tree(index, table, cx)
                        })))
                })))
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(anchored().position(*position).anchor(gpui::Anchor::TopLeft).child(menu.clone())).with_priority(1)
            }))
    }
}

struct ConnectionEditor {
    original: Option<ConnectionConfig>,
    id: String,
    saving: bool,
    panel: WeakEntity<DatabasePanel>,
    name: Entity<Editor>,
    host: Entity<Editor>,
    port: Entity<Editor>,
    user: Entity<Editor>,
    password: Entity<Editor>,
    database: Entity<Editor>,
    ssl_mode: Entity<Editor>,
    path: Entity<Editor>,
    ssh_host: Entity<Editor>,
    ssh_port: Entity<Editor>,
    ssh_user: Entity<Editor>,
    ssh_key: Entity<Editor>,
    ssh_secret: Entity<Editor>,
    kind: DatabaseKind,
    ssh_enabled: bool,
    ssh_password: bool,
    read_only: bool,
    color: Option<ConnectionColor>,
    project_scope: Option<Vec<PathBuf>>,
    project_roots: Vec<PathBuf>,
    error: Option<String>,
}

impl ConnectionEditor {
    fn new(
        panel: WeakEntity<DatabasePanel>,
        config: ConnectionConfig,
        project_roots: Vec<PathBuf>,
        is_new: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let ssh = config.ssh.clone().unwrap_or_default();
        let original = (!is_new).then(|| config.clone());
        Self {
            original,
            id: config.id,
            saving: false,
            panel,
            name: input("Connection name", &config.name, false, window, cx),
            host: input("Database host", &config.host, false, window, cx),
            port: input("Port", &config.port.to_string(), false, window, cx),
            user: input("User", &config.user, false, window, cx),
            password: input(
                "Password (empty keeps saved password)",
                "",
                true,
                window,
                cx,
            ),
            database: input("Database", &config.database, false, window, cx),
            ssl_mode: input(
                tls_placeholder(config.kind),
                config.ssl_mode.as_deref().unwrap_or_default(),
                false,
                window,
                cx,
            ),
            path: input(
                "Absolute path to SQLite database",
                &config
                    .path
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
                false,
                window,
                cx,
            ),
            ssh_host: input("SSH host", &ssh.host, false, window, cx),
            ssh_port: input("SSH port", &ssh.port.to_string(), false, window, cx),
            ssh_user: input("SSH user", &ssh.user, false, window, cx),
            ssh_key: input(
                "Private key path (empty uses SSH agent)",
                &ssh.key_path
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
                false,
                window,
                cx,
            ),
            ssh_secret: input("Key passphrase or SSH password", "", true, window, cx),
            kind: config.kind,
            ssh_enabled: config.ssh.is_some(),
            ssh_password: ssh.use_password,
            read_only: config.read_only,
            color: config.color,
            project_scope: config.project_scope,
            project_roots,
            error: None,
        }
    }

    fn save(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let result: Result<(ConnectionConfig, Secrets)> = (|| {
            let name = self.name.read(cx).text(cx).trim().to_owned();
            anyhow::ensure!(!name.is_empty(), "Enter a connection name");
            let sqlite = matches!(self.kind, DatabaseKind::Sqlite);
            let path = self.path.read(cx).text(cx).trim().to_owned();
            if sqlite {
                anyhow::ensure!(
                    PathBuf::from(&path).is_absolute(),
                    "Choose an absolute SQLite file path"
                );
            }
            let ssh = if self.ssh_enabled && !sqlite {
                let host = self.ssh_host.read(cx).text(cx).trim().to_owned();
                let user = self.ssh_user.read(cx).text(cx).trim().to_owned();
                anyhow::ensure!(
                    !host.is_empty() && !user.is_empty(),
                    "Enter SSH host and user"
                );
                Some(SshConfig {
                    host,
                    user,
                    port: parse_port(&self.ssh_port.read(cx).text(cx))?,
                    key_path: nonempty(self.ssh_key.read(cx).text(cx)).map(PathBuf::from),
                    use_password: self.ssh_password,
                    config_host: None,
                })
            } else {
                None
            };
            let config = ConnectionConfig {
                id: self.id.clone(),
                name,
                kind: self.kind,
                host: self.host.read(cx).text(cx).trim().to_owned(),
                port: if sqlite {
                    0
                } else {
                    parse_port(&self.port.read(cx).text(cx))?
                },
                user: self.user.read(cx).text(cx).trim().to_owned(),
                database: self.database.read(cx).text(cx).trim().to_owned(),
                path: sqlite.then(|| PathBuf::from(path)),
                ssl_mode: nonempty(self.ssl_mode.read(cx).text(cx).trim().to_owned()),
                ssh,
                read_only: self.read_only,
                color: self.color,
                project_scope: self.project_scope.clone(),
            };
            let secrets = Secrets {
                password: nonempty(self.password.read(cx).text(cx)),
                ssh_secret: nonempty(self.ssh_secret.read(cx).text(cx)),
            };
            Ok((config, secrets))
        })();
        match result {
            Ok((config, secrets)) => {
                let form = cx.entity().downgrade();
                match self.panel.update(cx, |panel, cx| {
                    panel.save_connection(config, self.original.clone(), secrets, form, cx)
                }) {
                    Ok(()) => {
                        self.saving = true;
                        self.error = None;
                    }
                    Err(error) => self.error = Some(error.to_string()),
                }
            }
            Err(error) => self.error = Some(error.to_string()),
        }
        cx.notify();
    }
}
impl Focusable for ConnectionEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}
impl EventEmitter<()> for ConnectionEditor {}
impl Item for ConnectionEditor {
    type Event = ();
    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        "Connection settings".into()
    }
    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Table))
    }
}
impl Render for ConnectionEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let fields = v_flex()
            .id("connection-editor-fields")
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll()
            .p_6()
            .gap_3()
            .child(Label::new("Database Connection").size(LabelSize::Large))
            .child(
                h_flex().gap_2().children(
                    [
                        (DatabaseKind::Postgres, "PostgreSQL", "5432"),
                        (DatabaseKind::MySql, "MySQL", "3306"),
                        (DatabaseKind::Sqlite, "SQLite", "0"),
                    ]
                    .into_iter()
                    .map(|(kind, label, port)| {
                        Button::new(label, label)
                            .toggle_state(self.kind == kind)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if this.kind != kind {
                                    this.ssl_mode.update(cx, |editor, cx| {
                                        editor.set_text("", window, cx);
                                        editor.set_placeholder_text(
                                            tls_placeholder(kind),
                                            window,
                                            cx,
                                        );
                                    });
                                }
                                this.kind = kind;
                                this.port
                                    .update(cx, |editor, cx| editor.set_text(port, window, cx));
                                cx.notify();
                            }))
                    }),
                ),
            )
            .child(field("Name", self.name.clone(), cx))
            .child(
                h_flex().gap_2().flex_wrap().child(Label::new("Color"))
                    .children(CONNECTION_COLORS.into_iter().map(|(color, label)| {
                        Button::new(label, label).start_icon(Icon::new(IconName::Circle).color(connection_color(color))).selected_label_color(connection_color(color))
                            .toggle_state(self.color == color)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.color = color;
                                cx.notify();
                            }))
                    })),
            )
            .child(
                h_flex().gap_2().child(Label::new("Visible in"))
                    .child(Button::new("scope-global", "All projects")
                        .toggle_state(self.project_scope.is_none())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.project_scope = None;
                            cx.notify();
                        })))
                    .child(Button::new("scope-project", "This project")
                        .disabled(self.project_roots.is_empty())
                        .toggle_state(self.project_scope.is_some())
                        .on_click(cx.listener(|this, _, _, cx| {
                            if !this.project_roots.is_empty() {
                                this.project_scope = Some(this.project_roots.clone());
                                cx.notify();
                            }
                        }))),
            )
            .when(self.project_roots.is_empty(), |view| {
                view.child(Label::new("Open a local project folder to limit connection visibility.").color(Color::Muted))
            })
            .when(matches!(self.kind, DatabaseKind::Sqlite), |view| {
                view.child(field("File", self.path.clone(), cx))
            })
            .when(!matches!(self.kind, DatabaseKind::Sqlite), |view| {
                view.child(
                    h_flex()
                        .gap_3()
                        .child(field("Host", self.host.clone(), cx))
                        .child(field("Port", self.port.clone(), cx)),
                )
                .child(field("Database", self.database.clone(), cx))
                .child(field("TLS mode", self.ssl_mode.clone(), cx))
                .child(
                    h_flex()
                        .gap_3()
                        .child(field("User", self.user.clone(), cx))
                        .child(field("Password", self.password.clone(), cx)),
                )
                .child(
                    Button::new(
                        "toggle-ssh",
                        if self.ssh_enabled {
                            "SSH tunnel: enabled"
                        } else {
                            "SSH tunnel: disabled"
                        },
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.ssh_enabled = !this.ssh_enabled;
                        cx.notify();
                    })),
                )
                .when(self.ssh_enabled, |view| {
                    view.child(
                        h_flex()
                            .gap_3()
                            .child(field("SSH host", self.ssh_host.clone(), cx))
                            .child(field("SSH port", self.ssh_port.clone(), cx)),
                    )
                    .child(field("SSH user", self.ssh_user.clone(), cx))
                    .child(
                        Button::new(
                            "ssh-auth",
                            if self.ssh_password {
                                "Authentication: password"
                            } else {
                                "Authentication: key / agent"
                            },
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.ssh_password = !this.ssh_password;
                            cx.notify();
                        })),
                    )
                    .when(!self.ssh_password, |view| {
                        view.child(field("Private key", self.ssh_key.clone(), cx))
                    })
                    .child(field(
                        "SSH password / key passphrase",
                        self.ssh_secret.clone(),
                        cx,
                    ))
                })
            })
            .child(
                Button::new("connection-access", if self.read_only { "Access: read-only" } else { "Access: read/write" })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.read_only = !this.read_only;
                        cx.notify();
                    })),
            )
            .child(
                Label::new(
                    "Passwords are stored in macOS Keychain. Table edits are saved with Apply; SQL runs immediately.",
                )
                .color(Color::Muted),
            );

        v_flex()
            .id("connection-editor")
            .size_full()
            .max_w(px(900.))
            .overflow_hidden()
            .child(fields)
            .child(
                v_flex()
                    .flex_none()
                    .gap_2()
                    .px_6()
                    .py_3()
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .when_some(self.error.clone(), |view, error| {
                        view.child(Label::new(error).color(Color::Error))
                    })
                    .child(
                        h_flex().child(
                            Button::new(
                                "save-connect",
                                if self.saving {
                                    "Saving…"
                                } else {
                                    "Save and Connect"
                                },
                            )
                            .style(ButtonStyle::Filled)
                            .disabled(self.saving)
                            .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                        ),
                    ),
            )
    }
}

struct DatabaseConsole {
    _appearance_subscription: Subscription,
    color: Option<ConnectionColor>,
    connection: DatabaseConnection,
    title: SharedString,
    editor: Entity<Editor>,
    language: Option<Arc<Language>>,
    table: Entity<TableView>,
    focus_handle: FocusHandle,
    status: String,
    error: Option<String>,
    running: bool,
    cancel_query: Option<QueryCancellation>,
    task: Option<Task<()>>,
}
impl DatabaseConsole {
    fn new(
        connection: DatabaseConnection,
        title: String,
        query: String,
        identifiers: Vec<String>,
        color: Option<ConnectionColor>,
        connection_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let sql_language = Language::new(
            LanguageConfig {
                name: "SQL".into(),
                code_fence_block_name: Some("sql".into()),
                line_comments: vec![Arc::from("-- ")],
                ..LanguageConfig::default()
            },
            Some(tree_sitter_sequel::LANGUAGE.into()),
        )
        .with_highlights_query(tree_sitter_sequel::HIGHLIGHTS_QUERY)
        .log_err()
        .map(Arc::new);
        if let Some(language) = &sql_language {
            language.set_theme(cx.theme().syntax());
        }
        let editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_text(query, window, cx);
            if let Some(buffer) = editor.active_buffer(cx) {
                buffer.update(cx, |buffer, cx| {
                    buffer.set_language(sql_language.clone(), cx)
                });
            }
            editor.set_completion_provider(Some(Rc::new(completion::SqlCompletionProvider::new(
                identifiers,
            ))));
            editor.set_show_completions_on_input(Some(true));
            editor
        });
        cx.on_release(|this, cx| {
            if let Some(cancel) = this.cancel_query.take() {
                cx.background_spawn(async move {
                    cancel.cancel().log_err();
                })
                .detach();
            }
        })
        .detach();
        let focus_handle = cx.focus_handle();
        let editor_focus = editor.focus_handle(cx);
        cx.on_focus(&focus_handle, window, move |_, window, cx| {
            editor_focus.focus(window, cx);
        })
        .detach();
        let appearance_subscription = cx.subscribe(
            &profile_events(cx),
            move |this, _, event: &ProfileAppearance, cx| {
                if event.id == connection_id {
                    this.color = event.color;
                    cx.emit(());
                    cx.notify();
                }
            },
        );
        Self {
            _appearance_subscription: appearance_subscription,
            color,
            connection,
            title: title.into(),
            editor,
            language: sql_language,
            table: cx.new(|cx| TableView::new(window, cx)),
            focus_handle,
            status: "Up to 1,000 rows · ⌘Enter to run".into(),
            error: None,
            running: false,
            cancel_query: None,
            task: None,
        }
    }
    fn run(&mut self, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        let editor = self.editor.read(cx);
        let selection = editor.selections.newest_anchor();
        let selected: String = editor
            .buffer()
            .read(cx)
            .snapshot(cx)
            .text_for_range(selection.range())
            .collect();
        let query = if selected.is_empty() {
            editor.text(cx)
        } else {
            selected
        };
        if query.trim().is_empty() {
            return;
        }
        let operation = match self.connection.start_query(&query, ROW_LIMIT) {
            Ok(operation) => operation,
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        self.cancel_query = Some(operation.cancel_handle());
        self.running = true;
        self.error = None;
        self.status = "Running…".into();
        self.table
            .update(cx, |table, cx| table.set_loading(true, cx));
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { operation.execute() })
                .await;
            this.update(cx, |this, cx| {
                this.running = false;
                this.cancel_query = None;
                match result {
                    Ok(result) => {
                        this.status = if let Some(affected_rows) = result.affected_rows {
                            format!(
                                "{affected_rows} affected rows · {} returned rows · {:.0} ms",
                                result.rows.len(),
                                result.execution_time.as_secs_f64() * 1000.
                            )
                        } else {
                            format!(
                                "{} rows · {:.0} ms{}",
                                result.rows.len(),
                                result.execution_time.as_secs_f64() * 1000.,
                                if result.row_limit_reached {
                                    " · limit reached"
                                } else {
                                    ""
                                }
                            )
                        };
                        this.table.update(cx, |table, cx| {
                            table.set_contents(table_contents(result), cx)
                        });
                    }
                    Err(error) => {
                        this.status = "Query failed".into();
                        this.error = Some(format!("{error:#}"));
                        this.table.update(cx, |table, cx| {
                            table.set_contents(TableLikeContent::default(), cx);
                            table.set_loading(false, cx);
                        });
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }
    fn cancel(&mut self, cx: &mut Context<Self>) {
        let Some(cancel) = self.cancel_query.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { cancel.cancel() }).await;
            if let Err(error) = result {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("Cancel failed: {error:#}"));
                    cx.notify();
                })
                .log_err();
            }
        })
        .detach();
    }
}
impl Focusable for DatabaseConsole {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
impl EventEmitter<()> for DatabaseConsole {}
impl Item for DatabaseConsole {
    type Event = ();
    fn to_item_events(_: &(), emit: &mut dyn FnMut(workspace::item::ItemEvent)) {
        emit(workspace::item::ItemEvent::UpdateTab);
    }
    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        self.title.clone()
    }
    fn tab_content(
        &self,
        params: workspace::item::TabContentParams,
        _: &Window,
        _: &App,
    ) -> gpui::AnyElement {
        h_flex()
            .gap_1()
            .child(
                Icon::new(IconName::Table)
                    .size(IconSize::Small)
                    .color(connection_color(self.color)),
            )
            .child(
                Label::new(self.title.clone())
                    .single_line()
                    .color(params.text_color()),
            )
            .into_any_element()
    }
}
impl Render for DatabaseConsole {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(language) = &self.language {
            language.set_theme(cx.theme().syntax());
        }
        v_flex()
            .size_full()
            .key_context("DatabaseConsole")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &RunQuery, _, cx| this.run(cx)))
            .on_action(cx.listener(|this, _: &CancelQuery, _, cx| this.cancel(cx)))
            .child(
                h_flex()
                    .p_2()
                    .gap_2()
                    .child(
                        Button::new("run-query", "Run")
                            .start_icon(Icon::new(IconName::PlayFilled))
                            .disabled(self.running)
                            .on_click(cx.listener(|this, _, _, cx| this.run(cx))),
                    )
                    .child(
                        Button::new("cancel-query", "Cancel")
                            .disabled(!self.running)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    )
                    .child(
                        Label::new(if self.connection.is_read_only() {
                            "Read-only"
                        } else {
                            "Read/write · SQL runs immediately"
                        })
                        .color(Color::Muted),
                    )
                    .child(Label::new(self.status.clone()).color(Color::Muted)),
            )
            .child(
                div()
                    .h(px(180.))
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(self.editor.clone()),
            )
            .when_some(self.error.clone(), |view, error| {
                view.child(div().p_3().child(Label::new(error).color(Color::Error)))
            })
            .child(div().flex_1().min_h_0().child(self.table.clone()))
    }
}

fn input(
    placeholder: &str,
    value: &str,
    masked: bool,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(placeholder, window, cx);
        editor.set_text(value, window, cx);
        editor.set_masked(masked, cx);
        editor
    })
}
fn field(label: &'static str, editor: Entity<Editor>, cx: &App) -> impl IntoElement {
    v_flex()
        .w_full()
        .min_w_0()
        .gap_1()
        .child(Label::new(label).color(Color::Muted))
        .child(
            div()
                .p_2()
                .border_1()
                .border_color(cx.theme().colors().border)
                .rounded_md()
                .child(editor),
        )
}
fn tls_placeholder(kind: DatabaseKind) -> &'static str {
    match kind {
        DatabaseKind::Postgres => "prefer / require / verify-full / disable (empty = default)",
        DatabaseKind::MySql => "PREFERRED / REQUIRED / VERIFY_IDENTITY / DISABLED",
        DatabaseKind::Sqlite => "",
    }
}
fn parse_port(text: &str) -> Result<u16> {
    let port: u16 = text
        .trim()
        .parse()
        .context("Port must be a number from 1 to 65535")?;
    anyhow::ensure!(port != 0, "Port must be a number from 1 to 65535");
    Ok(port)
}
fn nonempty(text: String) -> Option<String> {
    if text.is_empty() { None } else { Some(text) }
}
fn credential_key(id: &str, purpose: &str) -> String {
    format!("zed-db://connections/{id}/{purpose}")
}
fn load_connections() -> Result<Vec<ConnectionConfig>> {
    database_connection::load_profiles(&profiles_path())
}

fn table_contents(result: QueryResult) -> TableLikeContent {
    let count = result.columns.len();
    let row_count = result.rows.len();
    TableLikeContent {
        number_of_cols: count,
        headers: TableRow::from_vec(
            result
                .columns
                .into_iter()
                .map(|value| TableCell::Generated(value.into()))
                .collect(),
            count,
        ),
        rows: result
            .rows
            .into_iter()
            .map(|row| {
                TableRow::from_vec(
                    row.into_iter()
                        .map(|value| TableCell::Generated(value.into()))
                        .collect(),
                    count,
                )
            })
            .collect(),
        line_numbers: (1..=row_count).map(LineNumber::Line).collect(),
    }
}

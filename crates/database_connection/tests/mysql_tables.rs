use anyhow::{Context, Result};
use database_connection::{ConnectionConfig, DatabaseConnection, DatabaseKind, Secrets, TableInfo};

#[test]
#[ignore = "requires a disposable MySQL/MariaDB fixture on localhost at ZED_TEST_MYSQL_PORT"]
fn mysql_lists_and_browses_selected_database() -> Result<()> {
    let port = std::env::var("ZED_TEST_MYSQL_PORT")
        .context("Set ZED_TEST_MYSQL_PORT to the disposable fixture's localhost port")?
        .parse::<u16>()
        .context("ZED_TEST_MYSQL_PORT must be a valid TCP port")?;
    let mut config = ConnectionConfig {
        kind: DatabaseKind::MySql,
        host: "127.0.0.1".into(),
        port,
        user: "root".into(),
        database: "zed_table_discovery".into(),
        read_only: true,
        ..Default::default()
    };
    let connection = DatabaseConnection::connect(
        &config,
        Secrets {
            password: Some("zed-test".into()),
            ..Default::default()
        },
    )?;
    let table = TableInfo {
        schema: Some("zed_table_discovery".into()),
        name: "items".into(),
        is_view: false,
    };
    let view = TableInfo {
        schema: Some("zed_table_discovery".into()),
        name: "first_item".into(),
        is_view: true,
    };
    assert_eq!(connection.list_tables()?, [view.clone(), table.clone()]);

    let result = connection.browse(&table, 2, 1)?;
    assert_eq!(result.columns, ["id", "name"]);
    assert_eq!(result.rows, [vec!["2", "two"], vec!["3", "three"]]);
    let result = connection.browse(&view, 2, 0)?;
    assert_eq!(result.rows, [vec!["1", "one"]]);

    let snapshot = connection.start_browse(&table, 2, 1)?.execute()?;
    assert_eq!(snapshot.table, table);
    assert_eq!(snapshot.total_rows, 3);
    assert_eq!(snapshot.offset, 1);
    assert_eq!(snapshot.primary_key, [0]);
    assert_eq!(
        snapshot
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        ["id", "name"]
    );
    assert_eq!(snapshot.result.rows, [vec!["2", "two"], vec!["3", "three"]]);
    assert!(connection.is_read_only());
    connection.disconnect()?;

    config.database.clear();
    let connection = DatabaseConnection::connect(
        &config,
        Secrets {
            password: Some("zed-test".into()),
            ..Default::default()
        },
    )?;
    assert!(connection.list_tables()?.is_empty());
    assert_eq!(connection.query("SELECT 1 AS one", 1)?.rows, [vec!["1"]]);
    connection.disconnect()?;
    Ok(())
}

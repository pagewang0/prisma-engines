mod conversion;
mod error;

use crate::{
    ast::{ParameterizedValue, Query},
    connector::{metrics, queryable::*, ResultSet, DBIO},
    error::Error,
    visitor::{self, Visitor},
};
use futures::future;
use rusqlite::NO_PARAMS;
use std::{collections::HashSet, convert::TryFrom, path::Path, sync::Mutex, time::Duration};

const DEFAULT_SCHEMA_NAME: &str = "quaint";

/// A connector interface for the SQLite database
pub struct Sqlite {
    pub(crate) client: Mutex<rusqlite::Connection>,
    /// This is not a `PathBuf` because we need to `ATTACH` the database to the path, and this can
    /// only be done with UTF-8 paths.
    pub(crate) file_path: String,
}

pub struct SqliteParams {
    pub connection_limit: u32,
    /// This is not a `PathBuf` because we need to `ATTACH` the database to the path, and this can
    /// only be done with UTF-8 paths.
    pub file_path: String,
    pub db_name: String,
    pub socket_timeout: Duration,
}

type ConnectionParams = (Vec<(String, String)>, Vec<(String, String)>);

impl TryFrom<&str> for SqliteParams {
    type Error = Error;

    fn try_from(path: &str) -> crate::Result<Self> {
        let path = if path.starts_with("file:") {
            path.trim_start_matches("file:")
        } else {
            path.trim_start_matches("sqlite:")
        };

        let path_parts: Vec<&str> = path.split('?').collect();
        let path_str = path_parts[0];
        let path = Path::new(path_str);

        if path.is_dir() {
            Err(Error::DatabaseUrlIsInvalid(path.to_str().unwrap().to_string()))
        } else {
            let official = vec![];
            let mut connection_limit = num_cpus::get_physical() * 2 + 1;
            let mut db_name = None;
            let mut socket_timeout = Duration::from_secs(5);

            if path_parts.len() > 1 {
                let (_, unsupported): ConnectionParams = path_parts
                    .last()
                    .unwrap()
                    .split('&')
                    .map(|kv| {
                        let splitted: Vec<&str> = kv.split('=').collect();
                        (String::from(splitted[0]), String::from(splitted[1]))
                    })
                    .collect::<Vec<(String, String)>>()
                    .into_iter()
                    .partition(|(k, _)| official.contains(&k.as_str()));

                for (k, v) in unsupported.into_iter() {
                    match k.as_ref() {
                        "connection_limit" => {
                            let as_int: usize = v.parse().map_err(|_| Error::InvalidConnectionArguments)?;

                            connection_limit = as_int;
                        }
                        "db_name" => {
                            db_name = Some(v.to_string());
                        }
                        "socket_timeout" => {
                            let as_int = v.parse().map_err(|_| Error::InvalidConnectionArguments)?;
                            socket_timeout = Duration::from_secs(as_int);
                        }
                        _ => {
                            #[cfg(not(feature = "tracing-log"))]
                            trace!("Discarding connection string param: {}", k);
                            #[cfg(feature = "tracing-log")]
                            tracing::trace!(message = "Discarding connection string param", param = k.as_str());
                        }
                    };
                }
            }

            Ok(Self {
                connection_limit: u32::try_from(connection_limit).unwrap(),
                file_path: path_str.to_owned(),
                db_name: db_name.unwrap_or_else(|| DEFAULT_SCHEMA_NAME.to_owned()),
                socket_timeout,
            })
        }
    }
}

impl TryFrom<&str> for Sqlite {
    type Error = Error;

    fn try_from(path: &str) -> crate::Result<Self> {
        let params = SqliteParams::try_from(path)?;

        let conn = rusqlite::Connection::open_in_memory()?;
        conn.busy_timeout(params.socket_timeout)?;

        let client = Mutex::new(conn);
        let file_path = params.file_path;

        Ok(Sqlite { client, file_path, })
    }
}

impl Sqlite {
    pub fn new(file_path: &str) -> crate::Result<Sqlite> {
        Self::try_from(file_path)
    }

    pub fn attach_database(&mut self, db_name: &str) -> crate::Result<()> {
        let client = self.client.lock().unwrap();
        let mut stmt = client.prepare("PRAGMA database_list")?;

        let databases: HashSet<String> = stmt
            .query_map(NO_PARAMS, |row| {
                let name: String = row.get(1)?;

                Ok(name)
            })?
            .map(|res| res.unwrap())
            .collect();

        if !databases.contains(db_name) {
            rusqlite::Connection::execute(&client, "ATTACH DATABASE ? AS ?", &[self.file_path.as_str(), db_name])?;
        }

        rusqlite::Connection::execute(&client, "PRAGMA foreign_keys = ON", NO_PARAMS)?;

        Ok(())
    }
}

impl TransactionCapable for Sqlite {}

impl Queryable for Sqlite {
    fn query<'a>(&'a self, q: Query<'a>) -> DBIO<'a, ResultSet> {
        let (sql, params) = visitor::Sqlite::build(q);

        DBIO::new(async move { self.query_raw(&sql, &params).await })
    }

    fn query_raw<'a>(&'a self, sql: &'a str, params: &'a [ParameterizedValue]) -> DBIO<'a, ResultSet> {
        metrics::query("sqlite.query_raw", sql, params, move || {
            let res = move || {
                let client = self.client.lock().unwrap();
                let mut stmt = client.prepare_cached(sql)?;

                let mut rows = stmt.query(params)?;
                let mut result = ResultSet::new(rows.to_column_names(), Vec::new());

                while let Some(row) = rows.next()? {
                    result.rows.push(row.get_result_row()?);
                }

                result.set_last_insert_id(u64::try_from(client.last_insert_rowid()).unwrap_or(0));

                Ok(result)
            };

            match res() {
                Ok(res) => future::ok(res),
                Err(e) => future::err(e),
            }
        })
    }

    fn raw_cmd<'a>(&'a self, cmd: &'a str) -> DBIO<'a, ()> {
        metrics::query("sqlite.raw_cmd", cmd, &[], move || {
            let client = self.client.lock().unwrap();

            match client.execute_batch(cmd) {
                Ok(_) => future::ok(()),
                Err(e) => future::err(e.into()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ast::*, connector::{Queryable, TransactionCapable}, val, error::DatabaseConstraint};

    #[test]
    fn sqlite_params_from_str_should_resolve_path_correctly_with_file_scheme() {
        let path = "file:dev.db";
        let params = SqliteParams::try_from(path).unwrap();
        assert_eq!(params.file_path, "dev.db");
    }

    #[test]
    fn sqlite_params_from_str_should_resolve_path_correctly_with_sqlite_scheme() {
        let path = "sqlite:dev.db";
        let params = SqliteParams::try_from(path).unwrap();
        assert_eq!(params.file_path, "dev.db");
    }

    #[test]
    fn sqlite_params_from_str_should_resolve_path_correctly_with_no_scheme() {
        let path = "dev.db";
        let params = SqliteParams::try_from(path).unwrap();
        assert_eq!(params.file_path, "dev.db");
    }

    #[tokio::test]
    async fn should_provide_a_database_connection() {
        let connection = Sqlite::new("db/test.db").unwrap();
        let res = connection.query_raw("SELECT * FROM sqlite_master", &[]).await.unwrap();

        assert!(res.is_empty());
    }

    #[tokio::test]
    async fn should_provide_a_database_transaction() {
        let connection = Sqlite::new("db/test.db").unwrap();
        let tx = connection.start_transaction().await.unwrap();
        let res = tx.query_raw("SELECT * FROM sqlite_master", &[]).await.unwrap();

        assert!(res.is_empty());
    }

    #[allow(unused)]
    const TABLE_DEF: &str = r#"
    CREATE TABLE USER (
        ID INT PRIMARY KEY     NOT NULL,
        NAME           TEXT    NOT NULL,
        AGE            INT     NOT NULL,
        SALARY         REAL
    );
    "#;

    #[allow(unused)]
    const CREATE_USER: &str = r#"
    INSERT INTO USER (ID,NAME,AGE,SALARY)
    VALUES (1, 'Joe', 27, 20000.00 );
    "#;

    #[tokio::test]
    async fn should_map_columns_correctly() {
        let connection = Sqlite::try_from("file:db/test.db").unwrap();

        connection.query_raw(TABLE_DEF, &[]).await.unwrap();
        connection.query_raw(CREATE_USER, &[]).await.unwrap();

        let rows = connection.query_raw("SELECT * FROM USER", &[]).await.unwrap();
        assert_eq!(rows.len(), 1);

        let row = rows.get(0).unwrap();
        assert_eq!(row["ID"].as_i64(), Some(1));
        assert_eq!(row["NAME"].as_str(), Some("Joe"));
        assert_eq!(row["AGE"].as_i64(), Some(27));
        assert_eq!(row["SALARY"].as_f64(), Some(20000.0));
    }

    #[tokio::test]
    async fn op_test_add_one_level() {
        let connection = Sqlite::try_from("file:db/test.db").unwrap();
        let q = Select::default().value(val!(2) + val!(1));

        let rows = connection.select(q).await.unwrap();
        let row = rows.get(0).unwrap();

        assert_eq!(row[0].as_i64(), Some(3));
    }

    #[tokio::test]
    async fn op_test_add_two_levels() {
        let connection = Sqlite::try_from("file:db/test.db").unwrap();
        let q = Select::default().value(val!(2) + val!(val!(3) + val!(2)));

        let rows = connection.select(q).await.unwrap();
        let row = rows.get(0).unwrap();

        assert_eq!(row[0].as_i64(), Some(7));
    }

    #[tokio::test]
    async fn op_test_sub_one_level() {
        let connection = Sqlite::try_from("file:db/test.db").unwrap();
        let q = Select::default().value(val!(2) - val!(1));

        let rows = connection.select(q).await.unwrap();
        let row = rows.get(0).unwrap();

        assert_eq!(row[0].as_i64(), Some(1));
    }

    #[tokio::test]
    async fn op_test_sub_three_items() {
        let connection = Sqlite::try_from("file:db/test.db").unwrap();
        let q = Select::default().value(val!(2) - val!(1) - val!(1));

        let rows = connection.select(q).await.unwrap();
        let row = rows.get(0).unwrap();

        assert_eq!(row[0].as_i64(), Some(0));
    }

    #[tokio::test]
    async fn op_test_sub_two_levels() {
        let connection = Sqlite::try_from("file:db/test.db").unwrap();
        let q = Select::default().value(val!(2) - val!(val!(3) + val!(1)));

        let rows = connection.select(q).await.unwrap();
        let row = rows.get(0).unwrap();

        assert_eq!(row[0].as_i64(), Some(-2));
    }

    #[tokio::test]
    async fn op_test_mul_one_level() {
        let connection = Sqlite::try_from("file:db/test.db").unwrap();
        let q = Select::default().value(val!(6) * val!(6));

        let rows = connection.select(q).await.unwrap();
        let row = rows.get(0).unwrap();

        assert_eq!(row[0].as_i64(), Some(36));
    }

    #[tokio::test]
    async fn op_test_mul_two_levels() {
        let connection = Sqlite::try_from("file:db/test.db").unwrap();
        let q = Select::default().value(val!(6) * (val!(6) - val!(1)));

        let rows = connection.select(q).await.unwrap();
        let row = rows.get(0).unwrap();

        assert_eq!(row[0].as_i64(), Some(30));
    }

    #[tokio::test]
    async fn op_multiple_operations() {
        let connection = Sqlite::try_from("file:db/test.db").unwrap();
        let q = Select::default().value(val!(4) - val!(2) * val!(2));

        let rows = connection.select(q).await.unwrap();
        let row = rows.get(0).unwrap();

        assert_eq!(row[0].as_i64(), Some(0));
    }

    #[tokio::test]
    async fn op_test_div_one_level() {
        let connection = Sqlite::try_from("file:db/test.db").unwrap();
        let q = Select::default().value(val!(6) / val!(3));

        let rows = connection.select(q).await.unwrap();
        let row = rows.get(0).unwrap();

        assert_eq!(row[0].as_i64(), Some(2));
    }

    #[tokio::test]
    async fn test_uniq_constraint_violation() {
        let conn = Sqlite::try_from("file:db/test.db").unwrap();

        let _ = conn.raw_cmd("DROP TABLE test_uniq_constraint_violation").await;

        conn.raw_cmd("CREATE TABLE test_uniq_constraint_violation (id1 int, id2 int)").await.unwrap();
        conn.raw_cmd("CREATE UNIQUE INDEX musti ON test_uniq_constraint_violation (id1, id2)").await.unwrap();

        conn.query_raw(
            "INSERT INTO test_uniq_constraint_violation (id1, id2) VALUES (1, 2)",
            &[]
        ).await.unwrap();

        let res = conn.query_raw(
            "INSERT INTO test_uniq_constraint_violation (id1, id2) VALUES (1, 2)",
            &[]
        ).await;

        match res.unwrap_err() {
            Error::UniqueConstraintViolation { constraint } => {
                assert_eq!(
                    DatabaseConstraint::Fields(
                        vec![String::from("id1"), String::from("id2")]
                    ),
                    constraint,
                )
            },
            e => panic!(e)
        }
    }

    #[tokio::test]
    async fn test_null_constraint_violation() {
        let conn = Sqlite::try_from("file:db/test.db").unwrap();

        let _ = conn.raw_cmd("DROP TABLE test_null_constraint_violation").await;

        conn.raw_cmd("CREATE TABLE test_null_constraint_violation (id1 int not null, id2 int not null)").await.unwrap();

        let res = conn.query_raw(
            "INSERT INTO test_null_constraint_violation DEFAULT VALUES",
            &[]
        ).await;

        match res.unwrap_err() {
            Error::NullConstraintViolation { constraint } => {
                assert_eq!(DatabaseConstraint::Fields(vec![String::from("id1")]), constraint)
            },
            e => panic!(e)
        }
    }
}

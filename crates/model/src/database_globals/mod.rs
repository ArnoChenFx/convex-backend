use std::sync::LazyLock;

use common::{
    document::{
        ParseDocument,
        ParsedDocument,
    },
    query::{
        Order,
        Query,
    },
    runtime::Runtime,
};
use database::{
    ResolvedQuery,
    SystemMetadataModel,
    Transaction,
};
use migrations_model::DatabaseVersion;
use value::{
    TableName,
    TableNamespace,
};

use self::types::{
    DatabaseGlobals,
    StorageTagInitializer,
    StorageType,
};
use crate::{
    SystemIndex,
    SystemTable,
};

pub mod types;

pub static DATABASE_GLOBALS_TABLE: LazyLock<TableName> =
    LazyLock::new(|| "_db".parse().expect("invalid built-in db table"));

pub struct DatabaseGlobalsTable;
impl SystemTable for DatabaseGlobalsTable {
    type Metadata = DatabaseGlobals;

    fn table_name() -> &'static TableName {
        &DATABASE_GLOBALS_TABLE
    }

    fn indexes() -> Vec<SystemIndex<Self>> {
        vec![]
    }
}

pub struct DatabaseGlobalsModel<'a, RT: Runtime> {
    tx: &'a mut Transaction<RT>,
}

impl<'a, RT: Runtime> DatabaseGlobalsModel<'a, RT> {
    pub fn new(tx: &'a mut Transaction<RT>) -> Self {
        Self { tx }
    }

    pub async fn database_globals(&mut self) -> anyhow::Result<ParsedDocument<DatabaseGlobals>> {
        let metadata_query = Query::full_table_scan(DATABASE_GLOBALS_TABLE.clone(), Order::Asc);
        let mut query_stream = ResolvedQuery::new(self.tx, TableNamespace::Global, metadata_query)?;
        let globals: ParsedDocument<DatabaseGlobals> =
            match query_stream.expect_at_most_one(self.tx).await? {
                Some(globals) => globals.parse()?,
                None => anyhow::bail!("Database globals were not found??"),
            };
        Ok(globals)
    }

    pub async fn replace_database_globals(
        &mut self,
        database_globals: ParsedDocument<DatabaseGlobals>,
    ) -> anyhow::Result<()> {
        SystemMetadataModel::new_global(self.tx)
            .replace(
                database_globals.id(),
                database_globals.into_value().try_into()?,
            )
            .await?;
        Ok(())
    }

    pub async fn initialize(&mut self, initial_version: DatabaseVersion) -> anyhow::Result<()> {
        let aws_prefix_secret = self.tx.runtime().new_uuid_v4().to_string();
        let globals = DatabaseGlobals {
            version: initial_version,
            aws_prefix_secret,
            storage_type: None,
        };
        SystemMetadataModel::new_global(self.tx)
            .insert(&DATABASE_GLOBALS_TABLE, globals.try_into()?)
            .await?;
        Ok(())
    }

    pub async fn initialize_storage_tag(
        &mut self,
        storage_tag: StorageTagInitializer,
        instance_name: String,
    ) -> anyhow::Result<StorageType> {
        // Read the storage tag out of the DB and use it to create storage.
        let mut database_globals = self.database_globals().await?;

        // Make sure we start up with a storage configuration that matches the
        // database's configuration. Fail loudly if things don't match.
        let storage_type = match (&storage_tag, &database_globals.storage_type) {
            (
                StorageTagInitializer::Local { dir },
                Some(storage_type @ StorageType::Local { dir: db_dir }),
            ) => {
                // Allow switching from different local directories. This isn't common
                // but could happen if the directory is moved and then the backend gets
                // restarted with the new location.
                if dir.to_string_lossy() != db_dir.as_str() {
                    let new_storage_type = StorageType::Local {
                        dir: dir.to_string_lossy().into(),
                    };
                    tracing::info!(
                        "Switching storage tag from local dir {} to {}",
                        db_dir,
                        dir.to_string_lossy()
                    );
                    database_globals.storage_type = Some(new_storage_type.clone());
                    self.replace_database_globals(database_globals).await?;
                    new_storage_type
                } else {
                    storage_type.clone()
                }
            },
            (StorageTagInitializer::S3, Some(storage_type @ StorageType::S3 { s3_prefix })) => {
                anyhow::ensure!(
                    s3_prefix.starts_with(&format!("{instance_name}-")),
                    "Cannot use s3 storage path {s3_prefix} with {instance_name}"
                );
                storage_type.clone()
            },
            // DB not initialized yet. Initialize it
            (_, None) => {
                let storage_type = match storage_tag {
                    StorageTagInitializer::S3 => {
                        let s3_prefix_secret = self.tx.runtime().new_uuid_v4().to_string();
                        let s3_prefix = format!("{instance_name}-{s3_prefix_secret}/");
                        StorageType::S3 { s3_prefix }
                    },
                    StorageTagInitializer::Local { dir } => StorageType::Local {
                        dir: dir.to_string_lossy().into(),
                    },
                };
                database_globals.storage_type = Some(storage_type.clone());
                self.replace_database_globals(database_globals).await?;
                storage_type
            },
            (storage_tag, db_storage_type) => anyhow::bail!(
                "Database was initialized with {db_storage_type:?}, but backend started up with \
                 {storage_tag:?}."
            ),
        };
        Ok(storage_type)
    }
}

#[cfg(test)]
mod tests {
    use database::test_helpers::DbFixtures;
    use keybroker::DEV_INSTANCE_NAME;
    use runtime::testing::TestRuntime;
    use tempfile::TempDir;

    use crate::{
        database_globals::{
            types::{
                StorageTagInitializer,
                StorageType,
            },
            DatabaseGlobalsModel,
        },
        test_helpers::DbFixturesWithModel,
    };

    #[convex_macro::test_runtime]
    async fn test_allow_local_storage_switch(rt: TestRuntime) -> anyhow::Result<()> {
        let db = DbFixtures::new_with_model(&rt).await?.db;
        let dir = TempDir::new()?;
        let storage_tag = StorageTagInitializer::Local {
            dir: dir.path().to_owned(),
        };
        let mut tx = db.begin_system().await?;
        let mut db_model = DatabaseGlobalsModel::new(&mut tx);
        db_model
            .initialize_storage_tag(storage_tag.clone(), DEV_INSTANCE_NAME.into())
            .await?;
        // Ok to call twice
        db_model
            .initialize_storage_tag(storage_tag, DEV_INSTANCE_NAME.into())
            .await?;
        // Can't change storage paths
        let dir2 = TempDir::new()?;
        let new_storage_tag = StorageTagInitializer::Local {
            dir: dir2.path().to_owned(),
        };
        let storage_type = db_model
            .initialize_storage_tag(new_storage_tag, DEV_INSTANCE_NAME.into())
            .await?;
        match storage_type {
            StorageType::Local { dir: new_dir } => {
                anyhow::ensure!(
                    new_dir != dir.path().to_string_lossy(),
                    "Storage dir should be different"
                );
            },
            _ => anyhow::bail!("Storage type should be local"),
        };

        // Can't switch to s3 storage
        let storage_tag = StorageTagInitializer::S3;
        db_model
            .initialize_storage_tag(storage_tag, DEV_INSTANCE_NAME.into())
            .await
            .unwrap_err()
            .to_string()
            .contains("but backend started up with S3");
        Ok(())
    }

    #[convex_macro::test_runtime]
    async fn test_no_switching_storage_from_s3_to_local(rt: TestRuntime) -> anyhow::Result<()> {
        let db = DbFixtures::new_with_model(&rt).await?.db;
        let dir = TempDir::new()?;
        let mut tx = db.begin_system().await?;
        let mut db_model = DatabaseGlobalsModel::new(&mut tx);
        db_model
            .initialize_storage_tag(StorageTagInitializer::S3, DEV_INSTANCE_NAME.into())
            .await?;
        // Ok to call twice
        db_model
            .initialize_storage_tag(StorageTagInitializer::S3, DEV_INSTANCE_NAME.into())
            .await?;
        // Can't switch instance names
        db_model
            .initialize_storage_tag(StorageTagInitializer::S3, "new_instance_name".into())
            .await
            .unwrap_err()
            .to_string()
            .contains("Cannot use s3 storage path");
        // Cannot switch to local storage
        db_model
            .initialize_storage_tag(
                StorageTagInitializer::Local {
                    dir: dir.path().to_owned(),
                },
                DEV_INSTANCE_NAME.into(),
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("but backend started up with Local");
        Ok(())
    }
}

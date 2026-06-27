use crate::foundation::MigrationError;

pub const BASELINE_SCHEMA_VERSION: &str = "v0";
pub const SCHEMA_VERSION_TABLE: &str = "_dropbox_dev_schema_version";
pub const SCHEMA_VERSION_TABLE_SQL: &str = "CREATE TABLE IF NOT EXISTS _dropbox_dev_schema_version (version TEXT PRIMARY KEY, applied_at TEXT NOT NULL);";
pub const BASELINE_UP_SQL: &[&str] = &[SCHEMA_VERSION_TABLE_SQL];
pub const EMPTY_SQL: &[&str] = &[];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    pub version: &'static str,
    pub description: &'static str,
    pub up_sql: &'static [&'static str],
    pub down_sql: &'static [&'static str],
    pub product_tables: &'static [&'static str],
}

impl Migration {
    pub const fn new(
        version: &'static str,
        description: &'static str,
        up_sql: &'static [&'static str],
        down_sql: &'static [&'static str],
        product_tables: &'static [&'static str],
    ) -> Self {
        Self {
            version,
            description,
            up_sql,
            down_sql,
            product_tables,
        }
    }

    pub const fn baseline_v0() -> Self {
        Self {
            version: BASELINE_SCHEMA_VERSION,
            description: "empty baseline; creates only the schema version table",
            up_sql: BASELINE_UP_SQL,
            down_sql: EMPTY_SQL,
            product_tables: EMPTY_SQL,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    pub schema_version: String,
    pub product_tables: Vec<String>,
}

impl MigrationReport {
    pub fn product_table_count(&self) -> usize {
        self.product_tables.len()
    }
}

pub trait MigrationStore {
    fn ensure_schema_version_table(&mut self) -> Result<(), MigrationError>;
    fn schema_version(&self) -> Result<Option<String>, MigrationError>;
    fn set_schema_version(&mut self, version: &str) -> Result<(), MigrationError>;
    fn product_tables(&self) -> Result<Vec<String>, MigrationError>;

    fn apply_migration(&mut self, _migration: &Migration) -> Result<(), MigrationError> {
        Ok(())
    }

    fn rollback_migration(&mut self, _migration: &Migration) -> Result<(), MigrationError> {
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InMemoryMigrationStore {
    schema_version_table_created: bool,
    schema_version: Option<String>,
    product_tables: Vec<String>,
    applied_sql: Vec<&'static str>,
    rolled_back_sql: Vec<&'static str>,
}

impl InMemoryMigrationStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn applied_sql(&self) -> &[&'static str] {
        &self.applied_sql
    }

    pub fn rolled_back_sql(&self) -> &[&'static str] {
        &self.rolled_back_sql
    }
}

impl MigrationStore for InMemoryMigrationStore {
    fn ensure_schema_version_table(&mut self) -> Result<(), MigrationError> {
        self.schema_version_table_created = true;
        Ok(())
    }

    fn schema_version(&self) -> Result<Option<String>, MigrationError> {
        if !self.schema_version_table_created {
            return Ok(None);
        }
        Ok(self.schema_version.clone())
    }

    fn set_schema_version(&mut self, version: &str) -> Result<(), MigrationError> {
        if !self.schema_version_table_created {
            return Err(MigrationError::failed(
                "schema version table must be created before setting version",
            ));
        }
        self.schema_version = Some(version.to_owned());
        Ok(())
    }

    fn product_tables(&self) -> Result<Vec<String>, MigrationError> {
        Ok(self.product_tables.clone())
    }

    fn apply_migration(&mut self, migration: &Migration) -> Result<(), MigrationError> {
        self.applied_sql.extend(migration.up_sql.iter().copied());
        for &product_table in migration.product_tables {
            if !self
                .product_tables
                .iter()
                .any(|registered| registered.as_str() == product_table)
            {
                self.product_tables.push(product_table.to_owned());
            }
        }
        Ok(())
    }

    fn rollback_migration(&mut self, migration: &Migration) -> Result<(), MigrationError> {
        self.rolled_back_sql
            .extend(migration.down_sql.iter().copied());
        self.product_tables.retain(|registered| {
            !migration.product_tables.contains(&registered.as_str())
        });
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationRunner {
    migrations: Vec<Migration>,
}

impl MigrationRunner {
    pub fn empty_v0() -> Self {
        Self {
            migrations: vec![Migration::baseline_v0()],
        }
    }

    pub fn with_migrations(
        feature_migrations: impl IntoIterator<Item = Migration>,
    ) -> Result<Self, MigrationError> {
        let mut runner = Self::empty_v0();
        for migration in feature_migrations {
            runner.register_migration(migration)?;
        }
        Ok(runner)
    }

    pub fn register_migration(&mut self, migration: Migration) -> Result<(), MigrationError> {
        if migration.version.trim().is_empty() {
            return Err(MigrationError::failed("migration version must not be empty"));
        }

        if migration.version == BASELINE_SCHEMA_VERSION {
            return Err(MigrationError::failed(
                "feature migrations are layered after the v0 baseline",
            ));
        }

        if self
            .migrations
            .iter()
            .any(|registered| registered.version == migration.version)
        {
            return Err(MigrationError::failed(format!(
                "migration version `{}` is already registered",
                migration.version
            )));
        }

        self.migrations.push(migration);
        Ok(())
    }

    pub fn migrations(&self) -> &[Migration] {
        &self.migrations
    }

    pub fn apply(&self, store: &mut dyn MigrationStore) -> Result<MigrationReport, MigrationError> {
        let current_version = self.ensure_baseline_version(store)?;
        let current_index = self.migration_index(&current_version)?;

        for migration in self.migrations.iter().skip(current_index + 1) {
            store.apply_migration(migration)?;
            store.set_schema_version(migration.version)?;
        }

        self.report(store)
    }

    pub fn rollback(
        &self,
        store: &mut dyn MigrationStore,
    ) -> Result<MigrationReport, MigrationError> {
        let current_version = self.ensure_baseline_version(store)?;
        let current_index = self.migration_index(&current_version)?;
        if current_index == 0 {
            return self.report(store);
        }

        self.rollback_to(store, self.migrations[current_index - 1].version)
    }

    pub fn rollback_to(
        &self,
        store: &mut dyn MigrationStore,
        target_version: &str,
    ) -> Result<MigrationReport, MigrationError> {
        let current_version = self.ensure_baseline_version(store)?;
        let current_index = self.migration_index(&current_version)?;
        let target_index = self.migration_index(target_version)?;

        if target_index > current_index {
            return Err(MigrationError::failed(format!(
                "cannot rollback from `{}` to newer migration `{}`",
                current_version, target_version
            )));
        }

        for index in ((target_index + 1)..=current_index).rev() {
            store.rollback_migration(&self.migrations[index])?;
            store.set_schema_version(self.migrations[index - 1].version)?;
        }

        self.report(store)
    }

    fn ensure_baseline_version(
        &self,
        store: &mut dyn MigrationStore,
    ) -> Result<String, MigrationError> {
        store.ensure_schema_version_table()?;
        match store.schema_version()? {
            Some(version) => Ok(version),
            None => {
                store.set_schema_version(BASELINE_SCHEMA_VERSION)?;
                Ok(BASELINE_SCHEMA_VERSION.to_owned())
            }
        }
    }

    fn migration_index(&self, version: &str) -> Result<usize, MigrationError> {
        self.migrations
            .iter()
            .position(|migration| migration.version == version)
            .ok_or_else(|| MigrationError::failed(format!("unknown migration version `{version}`")))
    }

    fn report(&self, store: &dyn MigrationStore) -> Result<MigrationReport, MigrationError> {
        Ok(MigrationReport {
            schema_version: store
                .schema_version()?
                .unwrap_or_else(|| BASELINE_SCHEMA_VERSION.to_owned()),
            product_tables: store.product_tables()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG_UP_SQL: &[&str] = &["CREATE TABLE catalog_items (id TEXT PRIMARY KEY);"];
    const CATALOG_DOWN_SQL: &[&str] = &["DROP TABLE catalog_items;"];
    const CATALOG_TABLES: &[&str] = &["catalog_items"];

    fn catalog_migration() -> Migration {
        Migration::new(
            "catalog_v1",
            "catalog feature tables",
            CATALOG_UP_SQL,
            CATALOG_DOWN_SQL,
            CATALOG_TABLES,
        )
    }

    #[test]
    fn empty_baseline_reports_v0_without_product_tables() {
        let mut store = InMemoryMigrationStore::new();
        let report = MigrationRunner::empty_v0().apply(&mut store).unwrap();
        assert_eq!(report.schema_version, BASELINE_SCHEMA_VERSION);
        assert_eq!(report.product_table_count(), 0);
    }

    #[test]
    fn feature_migrations_register_apply_and_rollback_after_v0() {
        let runner = MigrationRunner::with_migrations([catalog_migration()]).unwrap();

        assert_eq!(runner.migrations()[0], Migration::baseline_v0());
        assert_eq!(runner.migrations()[1], catalog_migration());

        let mut store = InMemoryMigrationStore::new();
        let applied = runner.apply(&mut store).unwrap();

        assert_eq!(applied.schema_version, "catalog_v1");
        assert_eq!(applied.product_tables, vec!["catalog_items".to_owned()]);
        assert_eq!(store.applied_sql(), CATALOG_UP_SQL);

        let rolled_back = runner.rollback(&mut store).unwrap();

        assert_eq!(rolled_back.schema_version, BASELINE_SCHEMA_VERSION);
        assert_eq!(rolled_back.product_table_count(), 0);
        assert_eq!(store.rolled_back_sql(), CATALOG_DOWN_SQL);
    }

    #[test]
    fn feature_migration_registration_rejects_duplicate_versions() {
        let mut runner = MigrationRunner::empty_v0();
        runner.register_migration(catalog_migration()).unwrap();

        let error = runner.register_migration(catalog_migration()).unwrap_err();

        assert_eq!(error.code(), "MIGRATION_FAILED");
    }
}

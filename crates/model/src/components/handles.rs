use std::{
    collections::BTreeMap,
    sync::LazyLock,
};

use common::{
    bootstrap_model::components::handles::{
        FunctionHandle,
        FunctionHandleMetadata,
    },
    components::{
        CanonicalizedComponentFunctionPath,
        ComponentId,
    },
    document::{
        ParseDocument,
        ParsedDocument,
    },
    query::{
        IndexRange,
        IndexRangeExpression,
        Order,
        Query,
    },
    runtime::Runtime,
};
use database::{
    system_tables::{
        SystemIndex,
        SystemTable,
    },
    BootstrapComponentsModel,
    ResolvedQuery,
    SystemMetadataModel,
    Transaction,
};
use errors::ErrorMetadata;
use sync_types::{
    CanonicalizedModulePath,
    CanonicalizedUdfPath,
};
use value::{
    ConvexValue,
    DeveloperDocumentId,
    FieldPath,
    TableName,
    TableNamespace,
};

use crate::modules::module_versions::AnalyzedModule;

pub static FUNCTION_HANDLES_TABLE: LazyLock<TableName> = LazyLock::new(|| {
    "_function_handles"
        .parse()
        .expect("_function_handles is not a valid built-in table name")
});

pub static BY_COMPONENT_PATH_INDEX: LazyLock<SystemIndex<FunctionHandlesTable>> =
    LazyLock::new(|| {
        SystemIndex::new("by_component_path", [&COMPONENT_FIELD, &PATH_FIELD]).unwrap()
    });

pub static COMPONENT_FIELD: LazyLock<FieldPath> =
    LazyLock::new(|| "component".parse().expect("invalid component field"));

pub static PATH_FIELD: LazyLock<FieldPath> =
    LazyLock::new(|| "path".parse().expect("invalid path field"));

pub struct FunctionHandlesTable;

impl SystemTable for FunctionHandlesTable {
    type Metadata = FunctionHandleMetadata;

    fn table_name() -> &'static TableName {
        &FUNCTION_HANDLES_TABLE
    }

    fn indexes() -> Vec<SystemIndex<Self>> {
        vec![BY_COMPONENT_PATH_INDEX.clone()]
    }
}

pub fn function_handle_not_found() -> ErrorMetadata {
    ErrorMetadata::bad_request("FunctionHandleNotFound", "Function handle not found")
}

pub struct FunctionHandlesModel<'a, RT: Runtime> {
    tx: &'a mut Transaction<RT>,
}

impl<'a, RT: Runtime> FunctionHandlesModel<'a, RT> {
    pub fn new(tx: &'a mut Transaction<RT>) -> Self {
        Self { tx }
    }

    #[fastrace::trace]
    pub async fn lookup(
        &mut self,
        handle: FunctionHandle,
    ) -> anyhow::Result<CanonicalizedComponentFunctionPath> {
        let id = DeveloperDocumentId::from(handle);
        let resolved_id = self.tx.resolve_developer_id(&id, TableNamespace::Global)?;
        let Some(document) = self.tx.get(resolved_id).await? else {
            anyhow::bail!(function_handle_not_found());
        };
        let metadata = ParseDocument::<FunctionHandleMetadata>::parse(document)?.into_value();
        if metadata.deleted_ts.is_some() {
            anyhow::bail!(function_handle_not_found());
        }
        let component_path =
            BootstrapComponentsModel::new(self.tx).must_component_path(metadata.component)?;
        Ok(CanonicalizedComponentFunctionPath {
            component: component_path,
            udf_path: metadata.path,
        })
    }

    pub async fn get_with_component_path(
        &mut self,
        path: CanonicalizedComponentFunctionPath,
    ) -> anyhow::Result<FunctionHandle> {
        let Some((_, component_id)) =
            BootstrapComponentsModel::new(self.tx).component_path_to_ids(&path.component)?
        else {
            anyhow::bail!(ErrorMetadata::bad_request(
                "ComponentNotFound",
                "Component not found"
            ));
        };
        self.get(component_id, path.udf_path).await
    }

    pub async fn get(
        &mut self,
        component: ComponentId,
        path: CanonicalizedUdfPath,
    ) -> anyhow::Result<FunctionHandle> {
        let serialized_component = match component.serialize_to_string() {
            Some(s) => ConvexValue::String(s.try_into()?),
            None => ConvexValue::Null,
        };
        let index_range = IndexRange {
            index_name: BY_COMPONENT_PATH_INDEX.name(),
            range: vec![
                IndexRangeExpression::Eq(COMPONENT_FIELD.clone(), serialized_component.into()),
                IndexRangeExpression::Eq(
                    PATH_FIELD.clone(),
                    ConvexValue::String(String::from(path.clone()).try_into()?).into(),
                ),
            ],
            order: Order::Asc,
        };
        let query = Query::index_range(index_range);
        let mut query_stream = ResolvedQuery::new(self.tx, TableNamespace::Global, query)?;
        let Some(document) = query_stream.expect_at_most_one(self.tx).await? else {
            anyhow::bail!(function_handle_not_found())
        };
        let document: ParsedDocument<FunctionHandleMetadata> = document.parse()?;
        if document.deleted_ts.is_some() {
            anyhow::bail!(function_handle_not_found())
        }
        Ok(FunctionHandle::new(document.developer_id()))
    }

    #[fastrace::trace]
    pub async fn apply_config_diff(
        &mut self,
        component: ComponentId,
        // Set to `None` if we're deleting the component.
        new_analyze_results: Option<&BTreeMap<CanonicalizedModulePath, AnalyzedModule>>,
    ) -> anyhow::Result<()> {
        let serialized_component = match component.serialize_to_string() {
            Some(s) => ConvexValue::String(s.try_into()?),
            None => ConvexValue::Null,
        };
        let index_query = Query::index_range(IndexRange {
            index_name: BY_COMPONENT_PATH_INDEX.name(),
            range: vec![IndexRangeExpression::Eq(
                COMPONENT_FIELD.clone(),
                serialized_component.into(),
            )],
            order: Order::Asc,
        });
        let mut query_stream = ResolvedQuery::new(self.tx, TableNamespace::Global, index_query)?;

        let mut existing_handles = BTreeMap::new();
        while let Some(document) = query_stream.next(self.tx, None).await? {
            let document: ParsedDocument<FunctionHandleMetadata> = document.parse()?;
            anyhow::ensure!(document.component == component);
            anyhow::ensure!(existing_handles
                .insert(document.path.clone(), document)
                .is_none());
        }

        if let Some(new_analyze_results) = new_analyze_results {
            for (module_path, analyzed_module) in new_analyze_results {
                for function in &analyzed_module.functions {
                    let udf_path =
                        CanonicalizedUdfPath::new(module_path.clone(), function.name.clone());
                    match existing_handles.remove(&udf_path) {
                        Some(existing_handle) => {
                            if existing_handle.deleted_ts.is_some() {
                                let (id, mut metadata) = existing_handle.into_id_and_value();
                                metadata.deleted_ts = None;
                                SystemMetadataModel::new_global(self.tx)
                                    .replace(id, metadata.try_into()?)
                                    .await?;
                            }
                        },
                        None => {
                            let metadata = FunctionHandleMetadata {
                                component,
                                path: udf_path.clone(),
                                deleted_ts: None,
                            };
                            SystemMetadataModel::new_global(self.tx)
                                .insert(&FUNCTION_HANDLES_TABLE, metadata.try_into()?)
                                .await?;
                        },
                    }
                }
            }
        }

        for (_, remaining_handle) in existing_handles {
            let (id, mut metadata) = remaining_handle.into_id_and_value();
            if metadata.deleted_ts.is_none() {
                metadata.deleted_ts = Some(*self.tx.begin_timestamp());
                SystemMetadataModel::new_global(self.tx)
                    .replace(id, metadata.try_into()?)
                    .await?;
            }
        }

        Ok(())
    }
}

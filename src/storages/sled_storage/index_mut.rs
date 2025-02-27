use {
    super::{
        err_into, error::StorageError, index_sync::IndexSync, key, lock, SledStorage, Snapshot,
    },
    crate::{
        ast::OrderByExpr,
        data::{Schema, SchemaIndex, SchemaIndexOrd},
        result::{Error, MutResult, Result},
        store::{IndexMut, Store},
        IndexError,
    },
    async_trait::async_trait,
    sled::{
        transaction::{
            ConflictableTransactionError, ConflictableTransactionResult, TransactionError,
            TransactionalTree,
        },
        IVec,
    },
    std::iter::once,
};

macro_rules! try_self {
    ($self: expr, $expr: expr) => {
        match $expr {
            Err(e) => {
                return Err(($self, e.into()));
            }
            Ok(v) => v,
        }
    };
}

macro_rules! transaction {
    ($self: expr, $expr: expr) => {{
        let result = $self.tree.transaction($expr).map_err(|e| match e {
            TransactionError::Abort(e) => e,
            TransactionError::Storage(e) => StorageError::Sled(e).into(),
        });

        match result {
            Ok(_) => Ok(($self, ())),
            Err(e) => Err(($self, e)),
        }
    }};
}

fn fetch_schema(
    tree: &TransactionalTree,
    table_name: &str,
) -> ConflictableTransactionResult<(String, Option<Snapshot<Schema>>), Error> {
    let key = format!("schema/{}", table_name);
    let value = tree.get(&key.as_bytes())?;
    let schema_snapshot = value
        .map(|v| bincode::deserialize(&v))
        .transpose()
        .map_err(err_into)
        .map_err(ConflictableTransactionError::Abort)?;

    Ok((key, schema_snapshot))
}

#[async_trait(?Send)]
impl IndexMut<IVec> for SledStorage {
    async fn create_index(
        self,
        table_name: &str,
        index_name: &str,
        column: &OrderByExpr,
    ) -> MutResult<Self, ()> {
        let rows = try_self!(self, self.scan_data(table_name).await);
        let rows = try_self!(self, rows.collect::<Result<Vec<_>>>());

        let state = &self.state;

        transaction!(self, |tree| {
            let (txid, _) = lock::acquire(tree, state)?;

            let index_expr = &column.expr;

            let (schema_key, schema_snapshot) = fetch_schema(tree, table_name)?;
            let schema_snapshot = schema_snapshot
                .ok_or_else(|| IndexError::TableNotFound(table_name.to_string()).into())
                .map_err(ConflictableTransactionError::Abort)?;

            let (schema_snapshot, schema) = schema_snapshot.delete(txid);
            let Schema {
                column_defs,
                indexes,
                ..
            } = schema
                .ok_or_else(|| IndexError::ConflictTableNotFound(table_name.to_owned()).into())
                .map_err(ConflictableTransactionError::Abort)?;

            if indexes.iter().any(|index| index.name == index_name) {
                return Err(IndexError::IndexNameAlreadyExists(index_name.to_owned()).into())
                    .map_err(ConflictableTransactionError::Abort);
            }

            let index = SchemaIndex {
                name: index_name.to_owned(),
                expr: index_expr.clone(),
                order: SchemaIndexOrd::Both,
            };

            let indexes = indexes
                .into_iter()
                .chain(once(index.clone()))
                .collect::<Vec<_>>();

            let schema = Schema {
                table_name: table_name.to_owned(),
                column_defs,
                indexes,
            };

            let index_sync = IndexSync::from_schema(tree, txid, &schema);

            let schema_snapshot = schema_snapshot.update(txid, schema.clone());
            let schema_snapshot = bincode::serialize(&schema_snapshot)
                .map_err(err_into)
                .map_err(ConflictableTransactionError::Abort)?;

            for (data_key, row) in rows.iter() {
                index_sync.insert_index(&index, data_key, row)?;
            }

            tree.insert(schema_key.as_bytes(), schema_snapshot)?;

            let temp_key = key::temp_schema(txid, table_name);
            tree.insert(temp_key, schema_key.as_bytes())?;

            Ok(())
        })
    }

    async fn drop_index(self, table_name: &str, index_name: &str) -> MutResult<Self, ()> {
        let rows = try_self!(self, self.scan_data(table_name).await);
        let rows = try_self!(self, rows.collect::<Result<Vec<_>>>());

        let state = &self.state;

        transaction!(self, |tree| {
            let (txid, _) = lock::acquire(tree, state)?;

            let (schema_key, schema_snapshot) = fetch_schema(tree, table_name)?;
            let schema_snapshot = schema_snapshot
                .ok_or_else(|| IndexError::TableNotFound(table_name.to_string()).into())
                .map_err(ConflictableTransactionError::Abort)?;

            let (schema_snapshot, schema) = schema_snapshot.delete(txid);
            let Schema {
                column_defs,
                indexes,
                ..
            } = schema
                .ok_or_else(|| IndexError::ConflictTableNotFound(table_name.to_owned()).into())
                .map_err(ConflictableTransactionError::Abort)?;

            let (index, indexes): (Vec<_>, _) = indexes
                .into_iter()
                .partition(|index| index.name == index_name);

            let index = match index.into_iter().next() {
                Some(index) => index,
                None => {
                    return Err(IndexError::IndexNameDoesNotExist(index_name.to_owned()).into())
                        .map_err(ConflictableTransactionError::Abort);
                }
            };

            let schema = Schema {
                table_name: table_name.to_owned(),
                column_defs,
                indexes,
            };

            let index_sync = IndexSync::from_schema(tree, txid, &schema);

            let schema_snapshot = schema_snapshot.update(txid, schema.clone());
            let schema_snapshot = bincode::serialize(&schema_snapshot)
                .map_err(err_into)
                .map_err(ConflictableTransactionError::Abort)?;

            for (data_key, row) in rows.iter() {
                index_sync.delete_index(&index, data_key, row)?;
            }

            tree.insert(schema_key.as_bytes(), schema_snapshot)?;

            let temp_key = key::temp_schema(txid, table_name);
            tree.insert(temp_key, schema_key.as_bytes())?;

            Ok(())
        })
    }
}

/*
 * Copyright 2023 Amazon.com, Inc. or its affiliates. All Rights Reserved.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Integration between SQLite and Cedar entity stores

use std::collections::{HashMap, HashSet};

use cedar_policy::{
    EntityAttrAccessError, EntityId, EntityTypeName, EntityUid, EvaledEntity, PartialValue, Value,
};
use rusqlite::{
    types::{FromSql, ValueRef},
    Connection, OptionalExtension, Row,
};
use sea_query::{SelectStatement, SqliteQueryBuilder};
use smol_str::SmolStr;

use crate::sql_common::{
    make_ancestors, AncestorSQLInfo, DatabaseToCedarError, EntitySQLId, EntitySQLInfo,
    IsSQLDatabase, SQLValue,
};

impl FromSql for SQLValue {
    fn column_result(value: ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        match value {
            ValueRef::Null => Ok(SQLValue(None)),
            ValueRef::Integer(x) => Ok(SQLValue(Some(x.into()))),
            // TODO: use decimal type
            ValueRef::Real(_) => Err(rusqlite::types::FromSqlError::InvalidType),
            ValueRef::Text(s) => {
                let decoded = std::str::from_utf8(s)
                    .map_err(|_| rusqlite::types::FromSqlError::InvalidType)?;
                Ok(SQLValue(Some(decoded.into())))
            }
            ValueRef::Blob(_) => Err(rusqlite::types::FromSqlError::InvalidType),
        }
    }
}

impl FromSql for EntitySQLId {
    fn column_result(value: ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        match value {
            ValueRef::Integer(x) => Ok(EntitySQLId(x.to_string().parse().unwrap())),
            ValueRef::Text(_) => value.as_str().map(|v| EntitySQLId(v.parse().unwrap())),
            _ => Err(rusqlite::types::FromSqlError::InvalidType),
        }
    }
}

/// SQLite-specific SQL info
#[allow(missing_debug_implementations)]
pub struct SQLiteSQLInfo;

impl IsSQLDatabase for SQLiteSQLInfo {}

impl EntitySQLInfo<SQLiteSQLInfo> {
    /// Get all ancestors of an entity when the ancestors are stored in a column of the entity table
    pub fn make_entity_ancestors(
        &self,
        conn: &Connection,
        uid: &EntityUid,
    ) -> Result<Option<EvaledEntity>, DatabaseToCedarError> {
        self.make_entity(conn, uid, |row| match self.ancestor_attr_ind {
            Some(ancestors_attr_ind) => make_ancestors(serde_json::from_str(
                &row.get::<_, String>(ancestors_attr_ind)?,
            )?),
            None => panic!(
                "make_entity_ancestors should only be called when `ancestors_attr_ind` is filled"
            ),
        })
    }

    /// Get entity given a function which determines how to get ancestors given the information in the row
    pub fn make_entity(
        &self,
        conn: &Connection,
        uid: &EntityUid,
        ancestors: impl FnOnce(&Row<'_>) -> Result<HashSet<EntityUid>, DatabaseToCedarError>,
    ) -> Result<Option<EvaledEntity>, DatabaseToCedarError> {
        Self::make_entity_from_table(
            conn,
            uid,
            &self.get_select(uid.id()),
            |row| Self::convert_attr_names(&row, &self.attr_names_map),
            ancestors,
        )
    }

    /// Create an entity using `make_entity` and supply extra attributes that can depend on the row
    /// Useful for when some attributes are nontrivial functions of the data stored in the table
    pub fn make_entity_extra_attrs(
        &self,
        conn: &Connection,
        uid: &EntityUid,
        ancestors: impl FnOnce(&Row<'_>) -> Result<HashSet<EntityUid>, DatabaseToCedarError>,
        extra_attrs: impl FnOnce(
            &Row<'_>,
        ) -> Result<HashMap<String, PartialValue>, DatabaseToCedarError>,
    ) -> Result<Option<EvaledEntity>, DatabaseToCedarError> {
        Self::make_entity_from_table(
            conn,
            uid,
            &self.get_select(uid.id()),
            |row| {
                let mut attrs = Self::convert_attr_names(&row, &self.attr_names_map)?;
                attrs.extend(extra_attrs(row)?);
                Ok(attrs)
            },
            ancestors,
        )
    }

    /// Get a single attribute of an entity
    pub fn get_single_attr_as<T: FromSql>(
        &self,
        conn: &Connection,
        id: &EntityId,
        attr: &str,
    ) -> Result<T, EntityAttrAccessError<DatabaseToCedarError>> {
        let query = self
            .get_single_attr_select(id, attr)
            .ok_or(EntityAttrAccessError::UnknownAttr)?;
        let query_result: T = conn
            .query_row(&query.to_string(SqliteQueryBuilder), [], |row| row.get(0))
            .optional()
            .map_err(DatabaseToCedarError::from)?
            .ok_or(EntityAttrAccessError::UnknownEntity)?;
        Ok(query_result)
    }

    /// Get a single attribute of an entity as a cedar `Value`
    pub fn get_single_attr(
        &self,
        conn: &Connection,
        id: &EntityId,
        attr: &str,
    ) -> Result<Value, EntityAttrAccessError<DatabaseToCedarError>> {
        let query_result: SQLValue = self.get_single_attr_as(conn, id, attr)?;
        match query_result {
            SQLValue(Some(v)) => Ok(v),
            SQLValue(None) => Err(EntityAttrAccessError::UnknownAttr),
        }
    }

    /// Get a single attribute of an entity as a cedar `EntityUid`
    pub fn get_single_attr_as_id(
        &self,
        conn: &Connection,
        id: &EntityId,
        attr: &str,
        tp: EntityTypeName,
    ) -> Result<EntityUid, EntityAttrAccessError<DatabaseToCedarError>> {
        let query_result: EntitySQLId = self.get_single_attr_as(conn, id, attr)?;
        Ok(query_result.into_uid(tp))
    }

    /// Check whether the given entity exists
    pub fn exists_entity(
        &self,
        conn: &Connection,
        id: &EntityId,
    ) -> Result<bool, DatabaseToCedarError> {
        let query = self.get_exists_select(id);
        Ok(conn
            .query_row(&query.to_string(SqliteQueryBuilder), [], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// Convert a row into a map of attribute names to values
    pub fn convert_attr_names(
        query_result: &Row<'_>,
        attr_names: &HashMap<SmolStr, usize>,
    ) -> Result<HashMap<String, PartialValue>, DatabaseToCedarError> {
        attr_names
            .iter()
            .filter_map(|(nm, ind)| match query_result.get::<_, SQLValue>(*ind) {
                Ok(SQLValue(Some(v))) => Some(Ok((nm.to_string(), v.into()))),
                Ok(SQLValue(None)) => None,
                Err(e) => Some(Err(DatabaseToCedarError::from(e))),
            })
            .collect()
    }

    /// Construct an entity from a row in the entity table given a
    /// function which determines how to get ancestors given the information in the row
    /// and how to get attributes given the information in the row
    pub fn make_entity_from_table(
        conn: &Connection,
        uid: &EntityUid,
        query: &SelectStatement,
        attrs: impl FnOnce(&Row<'_>) -> Result<HashMap<String, PartialValue>, DatabaseToCedarError>,
        ancestors: impl FnOnce(&Row<'_>) -> Result<HashSet<EntityUid>, DatabaseToCedarError>,
    ) -> Result<Option<EvaledEntity>, DatabaseToCedarError> {
        // TODO: use `build` instead of `to_string`
        let query_string = query.to_string(SqliteQueryBuilder);
        Ok(conn
            .query_row(&query_string, [], |row| {
                Ok(EvaledEntity::new(
                    uid.clone(),
                    attrs(&row)
                        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
                    ancestors(&row)
                        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
                ))
            })
            .optional()?)
    }
}

impl AncestorSQLInfo<SQLiteSQLInfo> {
    /// Get all ancestors of an entity when the ancestry information is stored in a separate table
    pub fn get_ancestors(
        &self,
        conn: &Connection,
        id: &EntityId,
        tp: &EntityTypeName,
    ) -> Result<HashSet<EntityUid>, DatabaseToCedarError> {
        let mut stmt = conn.prepare(&self.query_all_parents(id).to_string(SqliteQueryBuilder))?;
        let result = stmt
            .query_map([], |row| {
                let parent_id: EntitySQLId = row.get(0)?;
                Ok(parent_id.into_uid(tp.clone()))
            })?
            .collect::<Result<HashSet<EntityUid>, _>>()?;
        Ok(result)
    }

    /// Check whether the given entity is a descendant of the given entity
    pub fn is_ancestor(
        &self,
        conn: &Connection,
        child_id: &EntityId,
        parent_id: &EntityId,
    ) -> Result<bool, DatabaseToCedarError> {
        Ok(conn
            .query_row(
                &self
                    .query_is_parent(child_id, parent_id)
                    .to_string(SqliteQueryBuilder),
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }
}

/*
pub struct EntitySQLInfo<'e> {
    pub table: &'e str,
    pub id_attr: &'e str,
    pub sql_attr_names: Vec<&'e str>,
    pub attr_names: Vec<(usize, &'e str)>,
    pub ancestor_attr_ind: Option<usize>,
    query_string: String
}

impl<'e> EntitySQLInfo<'e> {
    pub fn new(table: &'e str, id_attr: &'e str, sql_attr_names: Vec<&'e str>, attr_names: Vec<(usize, &'e str)>, ancestor_attr_ind: Option<usize>) -> Self {
        let attr_names_string: String =
            if sql_attr_names.is_empty() { "*".into() }
            else { sql_attr_names.iter().map(|x| format!("\"{}\"", x)).collect::<Vec<String>>().join(", ") };
        Self {
            table,
            id_attr,
            sql_attr_names,
            attr_names,
            ancestor_attr_ind,
            query_string: format!("SELECT {} FROM \"{}\" WHERE \"{}\" = ?", attr_names_string, table, id_attr)
        }
    }

    pub fn simple(table: &'e str, attr_names: Vec<&'e str>, ancestor_attr: Option<&'e str>) -> Self {
        let mut sql_attr_names: Vec<&'e str> = attr_names.clone();
        if let Some(ancestor_attr) = ancestor_attr {
            sql_attr_names.push(ancestor_attr);
        }

        let len = attr_names.len();
        let attr_names: Vec<(usize, &'e str)> = attr_names.into_iter().enumerate().collect();

        Self::new(table, "uid", sql_attr_names, attr_names, ancestor_attr.map(|_| len))
    }

    pub fn make_entity_ancestors(&self, conn: &Connection, uid: &EntityUid) -> Result<Option<ParsedEntity>, rusqlite::Error> {
        self.make_entity(conn, uid, |row| {
            match self.ancestor_attr_ind {
                Some(ancestors_attr_ind) => {
                    serde_json::from_str::<serde_json::Value>(&row.get::<_, String>(ancestors_attr_ind)?)
                    .map_err(|e| FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?
                    .as_array().ok_or(rusqlite::Error::InvalidQuery)? // TODO: make an error type
                    .iter()
                    .map(|x| {
                        EntityUid::from_json(x.clone()).map_err(|e| FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))
                    })
                    .collect()
                },
                None => panic!("make_entity_ancestors should only be called when `ancestors_attr_ind` is filled"),
            }
        })
    }

    pub fn get_query_string(&self) -> &str {
        return &self.query_string;
    }

    pub fn make_entity(&self, conn: &Connection, uid: &EntityUid, ancestors: impl FnOnce(&Row<'_>) -> Result<HashSet<EntityUid>, rusqlite::Error>)
        -> Result<Option<ParsedEntity>, rusqlite::Error> {
        make_entity_from_table(conn, uid, &self.query_string,
            |row| convert_attr_names(row, &self.attr_names),
            ancestors)
    }

    pub fn make_entity_extra_attrs(&self, conn: &Connection, uid: &EntityUid, ancestors: impl FnOnce(&Row<'_>) -> Result<HashSet<EntityUid>, rusqlite::Error>,
        extra_attrs: impl FnOnce(&Row<'_>) -> Result<HashMap<String, PartialValue>, rusqlite::Error>)
        -> Result<Option<ParsedEntity>, rusqlite::Error> {
        make_entity_from_table(conn, uid, &self.query_string,
            |row| {
                let mut attrs = convert_attr_names(row, &self.attr_names)?;
                attrs.extend(extra_attrs(row)?);
                Ok(attrs)
            }, ancestors)
    }

    pub fn get_single_attr_as<T: FromSql>(&self, conn: &Connection, id: &EntityId, attr: &str) -> Result<T, EntityAttrAccessError<rusqlite::Error>> {
        let attr_name = self.attr_names.iter()
            .find(|(_, s)| *s == attr)
            .and_then(|(i, _)| self.sql_attr_names.get(*i))
            .ok_or(EntityAttrAccessError::UnknownAttr)?;
        let query: String = format!("SELECT \"{}\" FROM \"{}\" WHERE \"{}\" = ?", attr_name, self.table, self.id_attr);
        conn.query_row(&query, &[&id.as_ref()], |row| row.get::<_, T>(0)).optional()?
            .ok_or(EntityAttrAccessError::UnknownEntity)
    }

    pub fn get_single_attr(&self, conn: &Connection, id: &EntityId, attr: &str) -> Result<Value, EntityAttrAccessError<rusqlite::Error>> {
        let query_result: SQLValue = self.get_single_attr_as(conn, id, attr)?;
        match query_result {
            SQLValue(Some(v)) => Ok(v),
            SQLValue(None) => Err(EntityAttrAccessError::UnknownAttr)
        }
    }

    pub fn get_single_attr_as_id(&self, conn: &Connection, id: &EntityId, attr: &str, tp: EntityTypeName) -> Result<Value, EntityAttrAccessError<rusqlite::Error>> {
        let query_result: EntitySQLId = self.get_single_attr_as(conn, id, attr)?;
        Ok(EntityUid::from_type_name_and_id(tp, query_result.id()).into())
    }

    pub fn exists_entity(&self, conn: &Connection, id: &EntityId) -> Result<bool, rusqlite::Error> {
        let query: String = format!("SELECT 1 FROM \"{}\" WHERE \"{}\" = ?", self.table, self.id_attr);
        Ok(conn.query_row(&query, &[&id.as_ref()], |_| Ok(())).optional()?.is_some())
    }
}

pub struct AncestorSQLInfo<'e> {
    pub table: &'e str,
    pub child_id: &'e str,
    pub parent_id: &'e str,
    query_all_string: String,
    query_one_string: String,
}

impl<'e> AncestorSQLInfo<'e> {
    pub fn new(table: &'e str, child_id: &'e str, parent_id: &'e str) -> Self {
        Self {
            table,
            child_id,
            parent_id,
            query_all_string: format!("SELECT \"{}\" FROM \"{}\" WHERE \"{}\" = ?", parent_id, table, child_id),
            query_one_string: format!("SELECT 1 FROM \"{}\" WHERE \"{}\" = ? AND \"{}\" = ?", table, child_id, parent_id),
        }
    }

    pub fn get_ancestors(&self, conn: &Connection, id: &EntityId, tp: &EntityTypeName) -> Result<HashSet<EntityUid>, rusqlite::Error> {
        let mut stmt = conn.prepare(&self.query_all_string)?;
        let result = stmt.query_map(&[id.as_ref()], |row| -> Result<EntityUid, Error> {
            let parent_id: EntitySQLId = row.get(0)?;
            Ok(EntityUid::from_type_name_and_id(tp.clone(), parent_id.0))
        });
        match result {
            Ok(x) => x.collect::<Result<HashSet<EntityUid>, Error>>(),
            Err(e) => Err(e),
        }
    }

    pub fn is_ancestor(&self, conn: &Connection, child_id: &EntityId, parent_id: &EntityId) -> Result<bool, rusqlite::Error> {
        conn.query_row(&self.query_one_string, &[child_id.as_ref(), parent_id.as_ref()], |_| Ok(()))
            .optional()
            .map(|x| x.is_some())
    }
}

pub fn convert_attr_names(query_result: &Row, attr_names: &[(usize, &str)]) -> Result<HashMap<String, PartialValue>, rusqlite::Error> {
    attr_names.iter()
        .filter_map(|(ind, nm)| {
            match query_result.get::<_, SQLValue>(*ind) {
                Ok(SQLValue(Some(v))) => Some(Ok((nm.to_string(), v.into()))),
                Ok(SQLValue(None)) => None,
                Err(e) => Some(Err(e)),
            }
        })
        .collect()
}

pub fn make_entity_from_table(conn: &Connection, uid: &EntityUid,
    query_string: &str,
    attrs: impl FnOnce(&Row<'_>) -> Result<HashMap<String, PartialValue>, rusqlite::Error>,
    ancestors: impl FnOnce(&Row<'_>) -> Result<HashSet<EntityUid>, rusqlite::Error>) -> Result<Option<ParsedEntity>, rusqlite::Error> {
    conn.query_row_and_then(query_string, &[uid.id().as_ref()], |row| {
        Ok::<ParsedEntity, rusqlite::Error>(ParsedEntity::new(uid.clone(), attrs(row)?, ancestors(row)?))
    })
    .optional()
}*/

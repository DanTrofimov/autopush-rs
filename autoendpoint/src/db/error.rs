use thiserror::Error;

use rusoto_core::RusotoError;
use rusoto_dynamodb::{
    DeleteItemError, DescribeTableError, GetItemError, PutItemError, UpdateItemError,
};

pub type DbResult<T> = Result<T, DbError>;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("Database error while performing GetItem")]
    GetItem(#[from] RusotoError<GetItemError>),

    #[error("Database error while performing UpdateItem")]
    UpdateItem(#[from] RusotoError<UpdateItemError>),

    #[error("Database error while performing PutItem")]
    PutItem(#[from] RusotoError<PutItemError>),

    #[error("Database error while performing DeleteItem")]
    DeleteItem(#[from] RusotoError<DeleteItemError>),

    #[error("Database error while performing DescribeTable")]
    DescribeTable(#[from] RusotoError<DescribeTableError>),

    #[error("Error while performing (de)serialization: {0}")]
    Serialization(#[from] serde_dynamodb::Error),

    #[error("Unable to determine table status")]
    TableStatusUnknown,
}

impl From<DbError> for autopush_common::db::error::DbError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::GetItem(e) => Self::DdbGetItem(e),
            DbError::UpdateItem(e) => Self::DdbUpdateItem(e),
            DbError::PutItem(e) => Self::DdbPutItem(e),
            DbError::DeleteItem(e) => Self::DdbDeleteItem(e),
            DbError::DescribeTable(e) => Self::DdbDescribeTable(e),
            DbError::Serialization(e) => Self::Serialization(e.to_string()),
            DbError::TableStatusUnknown => Self::TableStatusUnknown,
        }
    }
}

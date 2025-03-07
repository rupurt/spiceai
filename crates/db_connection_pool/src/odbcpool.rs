/*
Copyright 2024 The Spice.ai OSS Authors

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

     https://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use crate::dbconnection::odbcconn::ODBCConnection;
use crate::dbconnection::odbcconn::{ODBCDbConnection, ODBCParameter};
use async_trait::async_trait;
use odbc_api::{sys::AttrConnectionPooling, Connection, ConnectionOptions, Environment};
use secrecy::{ExposeSecret, Secret, SecretString};
use snafu::prelude::*;
use std::{collections::HashMap, sync::Arc};

use super::{DbConnectionPool, JoinPushDown, Result};
use lazy_static::lazy_static;

lazy_static! {
  static ref ENV: Environment = unsafe {
      // Enable connection pooling. Let driver decide wether the attributes of two connection
      // are similar enough to change the attributes of a pooled one, to fit the requested
      // connection, or if it is cheaper to create a new Connection from scratch.
      // See <https://docs.microsoft.com/en-us/sql/odbc/reference/develop-app/driver-aware-connection-pooling>
      if let Err(e) = Environment::set_connection_pooling(AttrConnectionPooling::DriverAware) {
          tracing::error!("Failed to set ODBC connection pooling: {e}");
      };
      match Environment::new() {
          Ok(env) => env,
          Err(e) => {
            panic!("Failed to create ODBC environment: {e}");
          }
      }
  };
}

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Missing ODBC connection string parameter: odbc_connection_string"))]
    MissingConnectionString {},

    #[snafu(display("Invalid parameter: {parameter_name}"))]
    InvalidParameterError { parameter_name: String },
}

pub struct ODBCPool {
    pool: &'static Environment,
    params: Arc<HashMap<String, SecretString>>,
    connection_string: String,
}

impl ODBCPool {
    // Creates a new instance of `ODBCPool`.
    ///
    /// # Errors
    ///
    /// Returns an error if there is a problem creating the connection pool.
    pub fn new(params: Arc<HashMap<String, SecretString>>) -> Result<Self> {
        let connection_string = params
            .get("odbc_connection_string")
            .map(Secret::expose_secret)
            .map(ToString::to_string)
            .context(MissingConnectionStringSnafu)?;
        Ok(Self {
            params,
            connection_string,
            pool: &ENV,
        })
    }

    #[must_use]
    pub fn odbc_environment(&self) -> &'static Environment {
        self.pool
    }
}

#[async_trait]
impl<'a> DbConnectionPool<Connection<'a>, ODBCParameter> for ODBCPool
where
    'a: 'static,
{
    async fn connect(&self) -> Result<Box<ODBCDbConnection<'a>>> {
        let cxn = self.pool.connect_with_connection_string(
            &self.connection_string,
            ConnectionOptions::default(),
        )?;

        let odbc_cxn = ODBCConnection {
            conn: Arc::new(cxn.into()),
            params: Arc::clone(&self.params),
        };

        Ok(Box::new(odbc_cxn))
    }

    fn join_push_down(&self) -> JoinPushDown {
        // It would be technically feasible to return JoinPushDown::AllowedFor(connection_string) here,
        // but we don't have a general way to strip out sensitive information from the connection string.
        // We could solve this by asking the user to explicly provide a join context in the parameters.
        JoinPushDown::Disallow
    }
}

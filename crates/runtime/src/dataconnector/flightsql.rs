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

use super::{DataConnector, DataConnectorFactory};
use crate::component::dataset::Dataset;
use crate::secrets::Secret;
use arrow_flight::sql::client::FlightSqlServiceClient;
use async_trait::async_trait;
use data_components::flightsql::FlightSQLFactory;
use data_components::Read;
use datafusion::datasource::TableProvider;
use flight_client::tls::new_tls_flight_channel;
use snafu::prelude::*;
use std::any::Any;
use std::collections::HashMap;
use std::pin::Pin;
use std::{future::Future, sync::Arc};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Missing required parameter: endpoint"))]
    MissingEndpointParameter,

    #[snafu(display("Unable to construct TLS flight client: {source}"))]
    UnableToConstructTlsChannel { source: flight_client::tls::Error },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone)]
pub struct FlightSQL {
    pub flightsql_factory: FlightSQLFactory,
}

impl DataConnectorFactory for FlightSQL {
    fn create(
        secret: Option<Secret>,
        params: Arc<HashMap<String, String>>,
    ) -> Pin<Box<dyn Future<Output = super::NewDataConnectorResult> + Send>> {
        Box::pin(async move {
            let endpoint: String = params
                .get("endpoint")
                .cloned()
                .context(MissingEndpointParameterSnafu)?;
            let flight_channel = new_tls_flight_channel(&endpoint)
                .await
                .context(UnableToConstructTlsChannelSnafu)?;

            let mut client = FlightSqlServiceClient::new(flight_channel);
            if let Some(s) = secret {
                let _ = client
                    .handshake(
                        s.get("username").unwrap_or_default(),
                        s.get("password").unwrap_or_default(),
                    )
                    .await;
            };
            let flightsql_factory = FlightSQLFactory::new(client, endpoint);
            Ok(Arc::new(Self { flightsql_factory }) as Arc<dyn DataConnector>)
        })
    }
}

#[async_trait]
impl DataConnector for FlightSQL {
    fn as_any(&self) -> &dyn Any {
        self
    }

    async fn read_provider(
        &self,
        dataset: &Dataset,
    ) -> super::DataConnectorResult<Arc<dyn TableProvider>> {
        Ok(
            Read::table_provider(&self.flightsql_factory, dataset.path().into())
                .await
                .context(super::UnableToGetReadProviderSnafu {
                    dataconnector: "flightsql",
                })?,
        )
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 NatureSense

//! DataSourceActor — owns the graph-backed `DataSource` definitions and
//! dispatches discovery/fetch to the registered drivers.
//!
//! Fetching runs the driver, then hands the GeoJSON payload to the shared
//! `ImportActor` pipeline (replace-by-name using the data source label as the
//! layer name), so a configured data source behaves like the existing
//! data.gov.sg import buttons.

use async_trait::async_trait;
use serde_json::Value;
use spire_actor::Actor;
use tokio::sync::{mpsc, oneshot};

use crate::actors::import::ImportMessage;
use crate::datasources::{
    DataSource, DataSourceDriver, DatasetInfo, Discovery, DriverRegistry,
    GraphDataSourceStore,
};

/// Messages for [`DataSourceActor`].
pub enum DataSourceMessage {
    /// Registered driver kinds (provider names), e.g. `["data-gov-sg"]`.
    Kinds {
        reply_to: oneshot::Sender<Result<Vec<String>, String>>,
    },
    List {
        reply_to: oneshot::Sender<Result<Vec<DataSource>, String>>,
    },
    Get {
        id: String,
        reply_to: oneshot::Sender<Result<Option<DataSource>, String>>,
    },
    /// Create a new definition for an existing driver kind.
    Add {
        kind: String,
        label: String,
        config: Value,
        reply_to: oneshot::Sender<Result<DataSource, String>>,
    },
    /// Patch an existing definition (label/config/enabled).
    Update {
        id: String,
        label: Option<String>,
        config: Option<Value>,
        enabled: Option<bool>,
        reply_to: oneshot::Sender<Result<DataSource, String>>,
    },
    Delete {
        id: String,
        reply_to: oneshot::Sender<Result<(), String>>,
    },
    /// Run the driver's `discover` for a definition's config; cache the
    /// summary on the definition (schema → DataSourceAttribute nodes).
    Discover {
        id: String,
        reply_to: oneshot::Sender<Result<DatasetInfo, String>>,
    },
    /// Run the driver's `fetch` and import the payload as a layer named after
    /// the data source label (replace-by-name).
    Fetch {
        id: String,
        reply_to: oneshot::Sender<Result<Value, String>>,
    },
}

/// Actor serving the data-source catalog + driver dispatch.
pub struct DataSourceActor {
    store: GraphDataSourceStore,
    drivers: DriverRegistry,
    import: mpsc::Sender<ImportMessage>,
}

impl DataSourceActor {
    pub fn new(
        graph: mpsc::Sender<spire_core::actors::MemoryGraphMessage>,
        drivers: DriverRegistry,
        import: mpsc::Sender<ImportMessage>,
    ) -> Self {
        Self {
            store: GraphDataSourceStore::new(graph),
            drivers,
            import,
        }
    }

    fn driver(&self, kind: &str) -> Result<std::sync::Arc<dyn DataSourceDriver>, String> {
        self.drivers.get(kind).ok_or_else(|| {
            format!(
                "unknown data source kind '{kind}'; available: {}",
                self.drivers.kinds().join(", ")
            )
        })
    }

    async fn add(
        &self,
        kind: String,
        label: String,
        config: Value,
    ) -> Result<DataSource, String> {
        // Validate the driver exists before persisting a definition.
        self.driver(&kind)?;
        let source = DataSource::new(kind, label, config);
        self.store.upsert(&source).await?;
        Ok(source)
    }

    async fn update(
        &self,
        id: String,
        label: Option<String>,
        config: Option<Value>,
        enabled: Option<bool>,
    ) -> Result<DataSource, String> {
        let mut source = self
            .store
            .get(&id)
            .await?
            .ok_or_else(|| format!("no data source with id '{id}'"))?;
        if let Some(label) = label {
            source.label = label;
        }
        if let Some(config) = config {
            source.config = config;
        }
        if let Some(enabled) = enabled {
            source.enabled = enabled;
        }
        source.touch();
        self.store.upsert(&source).await?;
        Ok(source)
    }


    async fn discover(&self, id: String) -> Result<DatasetInfo, String> {
        let source = self
            .store
            .get(&id)
            .await?
            .ok_or_else(|| format!("no data source with id '{id}'"))?;
        let driver = self.driver(&source.kind)?;
        let info = driver.discover(&source.config).await?;
        // Cache the summary on the definition (feature_count + geometry_types
        // scalar props; schema → DataSourceAttribute nodes).
        let mut updated = source;
        updated.discovered = Some(Discovery::from_info(&info));
        updated.touch();
        self.store.upsert(&updated).await?;
        Ok(info)
    }

    async fn fetch(&self, id: String) -> Result<Value, String> {
        let source = self
            .store
            .get(&id)
            .await?
            .ok_or_else(|| format!("no data source with id '{id}'"))?;
        let driver = self.driver(&source.kind)?;
        let payload = driver.fetch(&source.config).await?;
        let text = match payload {
            crate::datasources::DatasetPayload::GeoJsonText(text) => text,
        };
        let (t, r) = oneshot::channel();
        self.import
            .send(ImportMessage::ImportGeoJsonText {
                text,
                name: source.label.clone(),
                display_name: source.label.clone(),
                source: source.kind.clone(),
                reply_to: t,
            })
            .await
            .map_err(|e| format!("import actor gone: {e}"))?;
        let result = r
            .await
            .map_err(|e| format!("import reply lost: {e}"))?
            .map_err(|e| format!("import failed: {e}"))?;
        // Track the successful fetch time.
        let mut updated = source;
        updated.touch();
        self.store.upsert(&updated).await?;
        Ok(result)
    }
}

#[async_trait]
impl Actor for DataSourceActor {
    type Message = DataSourceMessage;

    async fn handle(&mut self, msg: Self::Message) {
        match msg {
            DataSourceMessage::Kinds { reply_to } => {
                let kinds: Vec<String> = self.drivers.kinds().into_iter().map(String::from).collect();
                let _ = reply_to.send(Ok(kinds));
            }
            DataSourceMessage::List { reply_to } => {
                let r = self.store.list().await;
                let _ = reply_to.send(r);
            }
            DataSourceMessage::Get { id, reply_to } => {
                let r = self.store.get(&id).await;
                let _ = reply_to.send(r);
            }
            DataSourceMessage::Add {
                kind,
                label,
                config,
                reply_to,
            } => {
                let r = self.add(kind, label, config).await;
                let _ = reply_to.send(r);
            }
            DataSourceMessage::Update {
                id,
                label,
                config,
                enabled,
                reply_to,
            } => {
                let r = self.update(id, label, config, enabled).await;
                let _ = reply_to.send(r);
            }
            DataSourceMessage::Delete { id, reply_to } => {
                let r = self.store.delete(&id).await;
                let _ = reply_to.send(r);
            }
            DataSourceMessage::Discover { id, reply_to } => {
                let r = self.discover(id).await;
                let _ = reply_to.send(r);
            }
            DataSourceMessage::Fetch { id, reply_to } => {
                let r = self.fetch(id).await;
                let _ = reply_to.send(r);
            }
        }
    }
}


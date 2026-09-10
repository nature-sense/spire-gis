//! End-to-end smoke tests for the gis/datasource/* RPCs (actor + graph store +
//! real import pipeline), using a deterministic mock driver (no network).

use serde_json::{json, Value};
use spire_actor::Actor;
use spire_core::actors::{LlmMessage, MemoryGraphActor, MemoryGraphMessage, TileActor, TileMessage};
use spire_gis::actors::import::{ImportActor, ImportMessage};
use spire_gis::actors::layer::{LayerActor, LayerMessage};
use spire_gis::coordinator::route_request;
use spire_gis::datasources::{
    analyze_geojson, DataSourceActor, DataSourceDriver, DataSourceMessage, DatasetInfo,
    DatasetPayload, DriverRegistry,
};
use tokio::sync::{mpsc, oneshot};

use async_trait::async_trait;

const SAMPLE_GEOJSON: &str = r#"{
  "type": "FeatureCollection",
  "features": [
    {"type":"Feature","properties":{"NAME":"Zone A","OBJECTID":1},
     "geometry":{"type":"Polygon","coordinates":[[[103.70,1.20],[103.90,1.20],[103.90,1.40],[103.70,1.40],[103.70,1.20]]]}},
    {"type":"Feature","properties":{"NAME":"Zone B","OBJECTID":2},
     "geometry":{"type":"Polygon","coordinates":[[[103.90,1.20],[104.10,1.20],[104.10,1.40],[103.90,1.40],[103.90,1.20]]]}},
    {"type":"Feature","properties":{"NAME":"Zone C","OBJECTID":3},
     "geometry":{"type":"Polygon","coordinates":[[[103.70,1.40],[103.90,1.40],[103.90,1.55],[103.70,1.55],[103.70,1.40]]]}}
  ]
}"#;

/// Deterministic driver: serves the sample fixture, no network.
struct MockDriver;

#[async_trait]
impl DataSourceDriver for MockDriver {
    fn kind(&self) -> &'static str {
        "mock"
    }

    async fn discover(&self, _config: &Value) -> Result<DatasetInfo, String> {
        analyze_geojson(SAMPLE_GEOJSON, "mock".to_string(), "mock fixture".to_string())
    }

    async fn fetch(&self, _config: &Value) -> Result<DatasetPayload, String> {
        Ok(DatasetPayload::GeoJsonText(SAMPLE_GEOJSON.to_string()))
    }
}

/// Real actor harness: graph + layer/import/datasource/tile actors in temp dir.
async fn harness(
    dir: &std::path::Path,
) -> (
    mpsc::Sender<MemoryGraphMessage>,
    mpsc::Sender<DataSourceMessage>,
    mpsc::Sender<ImportMessage>,
    mpsc::Sender<LayerMessage>,
    mpsc::Sender<TileMessage>,
) {
    let (graph_tx, rx) = mpsc::channel(64);
    let _g = MemoryGraphActor::new().spawn(rx);
    let (t, r) = oneshot::channel();
    graph_tx
        .send(MemoryGraphMessage::InitializeInMemory {
            data_dir: dir.to_path_buf(),
            reply_to: t,
        })
        .await
        .unwrap();
    r.await.unwrap().expect("store init");

    let (layer_tx, lrx) = mpsc::channel(64);
    let _l = LayerActor::new(graph_tx.clone()).spawn(lrx);
    let (import_tx, irx) = mpsc::channel(64);
    let _i = ImportActor::new(graph_tx.clone()).spawn(irx);
    let (ds_tx, drx) = mpsc::channel(64);
    let mut drivers = DriverRegistry::builtin();
    drivers.register(std::sync::Arc::new(MockDriver)).unwrap();
    let _d = DataSourceActor::new(graph_tx.clone(), drivers, import_tx.clone()).spawn(drx);
    let (tile_tx, trx) = mpsc::channel(64);
    let _t = TileActor::new(graph_tx.clone()).spawn(trx);

    (graph_tx, ds_tx, import_tx, layer_tx, tile_tx)
}

/// A disconnected sender: `send().await` fails immediately, so RPCs that never
/// reach a real actor return a clean error instead of waiting forever.
fn dead_channel<T>() -> mpsc::Sender<T> {
    let (tx, rx) = mpsc::channel::<T>(1);
    drop(rx);
    tx
}

async fn rpc(
    graph: &mpsc::Sender<MemoryGraphMessage>,
    ds: &mpsc::Sender<DataSourceMessage>,
    import: &mpsc::Sender<ImportMessage>,
    layers: &mpsc::Sender<LayerMessage>,
    tiles: &mpsc::Sender<TileMessage>,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let llm_tx = dead_channel::<LlmMessage>();
    route_request(graph, layers, import, tiles, &llm_tx, ds, method, &params).await
}


#[tokio::test]
async fn datasource_crud_discover_and_fetch_import() {
    let dir = tempfile::tempdir().unwrap();
    let (g, ds, import, layer_tx, tile_tx) = harness(dir.path()).await;

    // Builtin + mock drivers are registered.
    let kinds = rpc(&g, &ds, &import, &layer_tx, &tile_tx, "gis/datasource/kinds", json!({}))
        .await
        .expect("kinds");
    let kindNames: Vec<&str> = kinds
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|k| k.as_str())
        .collect();
    assert!(kindNames.contains(&"data-gov-sg"), "kinds: {kindNames:?}");
    assert!(kindNames.contains(&"mock"), "kinds: {kindNames:?}");

    // Add a definition.
    let added = rpc(
        &g,
        &ds,
        &import,
        &layer_tx,
        &tile_tx,
        "gis/datasource/add",
        json!({ "kind": "mock", "label": "Demo", "config": { "dataset_id": "d_x" } }),
    )
    .await
    .expect("add");
    let id = added["id"].as_str().unwrap().to_string();
    assert_eq!(added["kind"], "mock");
    assert_eq!(added["label"], "Demo");
    assert_eq!(added["config"]["dataset_id"], "d_x");

    // Unknown driver kinds are rejected.
    let err = rpc(
        &g,
        &ds,
        &import,
        &layer_tx,
        &tile_tx,
        "gis/datasource/add",
        json!({ "kind": "nope", "label": "X", "config": {} }),
    )
    .await
    .expect_err("unknown kind must fail");
    assert!(err.contains("unknown data source kind"), "got {err}");

    // List + get.
    let list = rpc(&g, &ds, &import, &layer_tx, &tile_tx, "gis/datasource/list", json!({}))
        .await
        .expect("list");
    assert_eq!(list.as_array().unwrap().len(), 1);
    let got = rpc(&g, &ds, &import, &layer_tx, &tile_tx, "gis/datasource/get", json!({ "id": id }))
        .await
        .expect("get");
    assert_eq!(got["label"], "Demo");

    // Update label.
    let updated = rpc(
        &g,
        &ds,
        &import,
        &layer_tx,
        &tile_tx,
        "gis/datasource/update",
        json!({ "id": id, "label": "Renamed" }),
    )
    .await
    .expect("update");
    assert_eq!(updated["id"], id);
    assert_eq!(updated["label"], "Renamed");

    // Discover: metadata + schema cached on the definition.
    let info = rpc(&g, &ds, &import, &layer_tx, &tile_tx, "gis/datasource/discover", json!({ "id": id }))
        .await
        .expect("discover");
    assert_eq!(info["feature_count"], 3);
    assert_eq!(info["geometry_types"], json!(["Polygon"]));
    assert_eq!(info["schema"]["NAME"], "string");
    let got2 = rpc(&g, &ds, &import, &layer_tx, &tile_tx, "gis/datasource/get", json!({ "id": id }))
        .await
        .expect("get after discover");
    assert_eq!(got2["discovered"]["feature_count"], 3);
    assert_eq!(got2["discovered"]["schema"]["NAME"], "string");

    // Fetch: driver payload is imported through the real import pipeline as a
    // layer named after the (updated) label.
    let report = rpc(&g, &ds, &import, &layer_tx, &tile_tx, "gis/datasource/fetch", json!({ "id": id }))
        .await
        .expect("fetch");
    assert_eq!(report["name"], "Renamed");
    assert_eq!(report["feature_count"], 3);

    // The imported layer shows up in the catalog.
    let layers = rpc(&g, &ds, &import, &layer_tx, &tile_tx, "gis/list-layers", json!({}))
        .await
        .expect("list layers");
    let names: Vec<&str> = layers
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|l| l["name"].as_str())
        .collect();
    assert!(names.contains(&"Renamed"), "layers: {names:?}");

    // get-feature returns the full attribute map for one stored feature.
    let fc = rpc(
        &g,
        &ds,
        &import,
        &layer_tx,
        &tile_tx,
        "gis/get-layer-geojson",
        json!({ "layer": "Renamed", "limit": 10 }),
    )
    .await
    .expect("layer geojson");
    // Pick the Zone A feature (order is not guaranteed by the query).
    let feat = fc["features"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["properties"]["NAME"] == "Zone A")
        .expect("zone A feature");
    let feat_id = feat["id"].as_str().expect("feature id").to_string();
    let detail = rpc(
        &g,
        &ds,
        &import,
        &layer_tx,
        &tile_tx,
        "gis/get-feature",
        json!({ "id": feat_id }),
    )
    .await
    .expect("get-feature");
    assert_eq!(detail["id"], feat_id);
    assert_eq!(detail["layer"], "Renamed");
    assert_eq!(detail["attributes"]["NAME"], "Zone A");
    assert_eq!(detail["attributes"]["OBJECTID"], 1);
    assert!(detail["attributes"].get("geometry").is_none());
    assert!(detail["attributes"].get("layer_id").is_none());

    // Delete cascades the definition.
    rpc(&g, &ds, &import, &layer_tx, &tile_tx, "gis/datasource/delete", json!({ "id": id }))
        .await
        .expect("delete");
    let list2 = rpc(&g, &ds, &import, &layer_tx, &tile_tx, "gis/datasource/list", json!({}))
        .await
        .expect("list after delete");
    assert!(list2.as_array().unwrap().is_empty());
}


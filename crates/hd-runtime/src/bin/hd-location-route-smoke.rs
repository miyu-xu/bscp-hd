use std::path::PathBuf;

use anyhow::{Context as _, Result, ensure};
use hd_core::{MAX_LOCATION_ROUTE_POINTS_V2, MIN_LOCATION_ROUTE_INTERVAL_MS_V2};
use hd_runtime::{LocationRouteImportError, load_location_route};
use uuid::Uuid;

fn main() -> Result<()> {
    let temporary = SmokeDirectory::create()?;
    let gpx = temporary.write(
        "route.gpx",
        br#"<?xml version="1.0"?><gpx version="1.1"><trk><trkseg><trkpt lat="37.4219999" lon="-122.0840577"><ele>5.25</ele></trkpt><trkpt lat="37.4221000" lon="-122.0839000"><ele>6</ele></trkpt></trkseg></trk></gpx>"#,
    )?;
    let gpx_route = load_location_route(&gpx, 1_000, false)?;
    ensure!(gpx_route.points.len() == 2, "GPX point count mismatch");
    ensure!(
        gpx_route.points[0].latitude_e7 == 374_219_999
            && gpx_route.points[0].longitude_e7 == -1_220_840_577
            && gpx_route.points[0].altitude_mm == 5_250,
        "GPX coordinate conversion mismatch"
    );

    let kml = temporary.write(
        "route.kml",
        br#"<kml xmlns="http://www.opengis.net/kml/2.2"><Document><Placemark><LineString><coordinates>-122.0840577,37.4219999,5.25 -122.0839,37.4221,6</coordinates></LineString></Placemark></Document></kml>"#,
    )?;
    let kml_route = load_location_route(&kml, 500, true)?;
    ensure!(
        kml_route.points == gpx_route.points,
        "KML and GPX coordinate contracts differ: kml={:?} gpx={:?}",
        kml_route.points,
        gpx_route.points
    );
    ensure!(kml_route.repeat, "KML repeat option was not retained");

    let declaration = temporary.write(
        "declaration.gpx",
        br#"<!DOCTYPE gpx [<!ENTITY x "37">]><gpx><trk><trkseg><trkpt lat="&x;" lon="1"/><trkpt lat="2" lon="3"/></trkseg></trk></gpx>"#,
    )?;
    ensure!(
        matches!(
            load_location_route(&declaration, 1_000, false),
            Err(LocationRouteImportError::XmlDeclaration)
        ),
        "XML document type was not rejected"
    );
    ensure!(
        matches!(
            load_location_route(
                &gpx,
                MIN_LOCATION_ROUTE_INTERVAL_MS_V2.saturating_sub(1),
                false
            ),
            Err(LocationRouteImportError::Interval)
        ),
        "out-of-contract playback interval was not rejected"
    );

    let coordinates = (0..MAX_LOCATION_ROUTE_POINTS_V2)
        .map(|index| {
            format!(
                "{},1,0",
                f64::from(u32::try_from(index).unwrap_or(0)) / 100_000.0
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let maximum = temporary.write(
        "maximum.kml",
        format!("<kml><coordinates>{coordinates}</coordinates></kml>").as_bytes(),
    )?;
    let maximum_route = load_location_route(&maximum, 250, false)?;
    let serialized_bytes = serde_json::to_vec(&maximum_route)?.len();
    ensure!(
        serialized_bytes < 1024 * 1024,
        "maximum route exceeds the Worker IPC message limit"
    );

    let oversized = temporary.write(
        "oversized.kml",
        format!("<kml><coordinates>{coordinates} 2,2,0</coordinates></kml>").as_bytes(),
    )?;
    ensure!(
        matches!(
            load_location_route(&oversized, 250, false),
            Err(LocationRouteImportError::Points)
        ),
        "oversized route point set was not rejected"
    );

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "schema_version": 1,
            "gate": "location-route-smoke",
            "status": "pass",
            "formats": ["gpx", "kml"],
            "point_limit": MAX_LOCATION_ROUTE_POINTS_V2,
            "maximum_serialized_bytes": serialized_bytes,
            "xml_doctype_rejected": true,
            "invalid_interval_rejected": true,
            "oversized_route_rejected": true
        }))?
    );
    Ok(())
}

struct SmokeDirectory {
    path: PathBuf,
}

impl SmokeDirectory {
    fn create() -> Result<Self> {
        let temporary_root = std::env::temp_dir()
            .canonicalize()
            .context("canonicalize system temporary directory")?;
        let path = temporary_root.join(format!("hd-location-route-smoke-{}", Uuid::new_v4()));
        hd_platform::ensure_owner_only_directory(&path)
            .with_context(|| format!("create smoke directory {}", path.display()))?;
        Ok(Self { path })
    }

    fn write(&self, name: &str, bytes: &[u8]) -> Result<PathBuf> {
        let path = self.path.join(name);
        hd_platform::write_owner_only(&path, bytes)
            .with_context(|| format!("write smoke route {}", path.display()))?;
        Ok(path)
    }
}

impl Drop for SmokeDirectory {
    fn drop(&mut self) {
        if let Ok(entries) = std::fs::read_dir(&self.path) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.parent() == Some(self.path.as_path()) {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        let _ = std::fs::remove_dir(&self.path);
    }
}

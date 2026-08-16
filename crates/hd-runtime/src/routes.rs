use std::path::Path;

use hd_core::{
    LocationRouteV2, LocationV2, MAX_LOCATION_ROUTE_INTERVAL_MS_V2, MAX_LOCATION_ROUTE_POINTS_V2,
    MIN_LOCATION_ROUTE_INTERVAL_MS_V2,
};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::QName;
use thiserror::Error;
use uuid::Uuid;

const MAX_LOCATION_ROUTE_BYTES: u64 = 2 * 1024 * 1024;
const DEFAULT_ACCURACY_MM: u32 = 5_000;

#[derive(Debug, Error)]
pub enum LocationRouteImportError {
    #[error("read location route: {0}")]
    Read(#[from] hd_platform::PlatformError),
    #[error(
        "location route interval must be {MIN_LOCATION_ROUTE_INTERVAL_MS_V2}-{MAX_LOCATION_ROUTE_INTERVAL_MS_V2} ms"
    )]
    Interval,
    #[error("location route file name is invalid")]
    FileName,
    #[error("location route XML is malformed: {0}")]
    Xml(String),
    #[error("location route document types and entities are not allowed")]
    XmlDeclaration,
    #[error("location route format is not GPX or KML")]
    Format,
    #[error("location route must contain 2-{MAX_LOCATION_ROUTE_POINTS_V2} valid points")]
    Points,
}

pub fn load_location_route(
    path: &Path,
    interval_ms: u32,
    repeat: bool,
) -> Result<LocationRouteV2, LocationRouteImportError> {
    if !(MIN_LOCATION_ROUTE_INTERVAL_MS_V2..=MAX_LOCATION_ROUTE_INTERVAL_MS_V2)
        .contains(&interval_ms)
    {
        return Err(LocationRouteImportError::Interval);
    }
    let bytes = hd_platform::read_regular_nofollow_limited(path, MAX_LOCATION_ROUTE_BYTES)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(LocationRouteImportError::FileName)?
        .trim();
    if name.is_empty() || name.len() > 128 {
        return Err(LocationRouteImportError::FileName);
    }
    let points = parse_location_route(&bytes)?;
    let route = LocationRouteV2 {
        id: Uuid::new_v4(),
        name: name.to_owned(),
        points,
        interval_ms,
        repeat,
    };
    if !route.is_valid() {
        return Err(LocationRouteImportError::Points);
    }
    Ok(route)
}

fn parse_location_route(bytes: &[u8]) -> Result<Vec<LocationV2>, LocationRouteImportError> {
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut root = None;
    let mut points = Vec::new();
    let mut gpx_point = None;
    let mut in_gpx_ele = false;
    let mut in_kml_coordinates = false;

    loop {
        match reader
            .read_event()
            .map_err(|error| LocationRouteImportError::Xml(error.to_string()))?
        {
            Event::Start(start) => {
                let local = start.local_name();
                let local = local.as_ref();
                root.get_or_insert_with(|| local.to_vec());
                if local.eq_ignore_ascii_case(b"trkpt") || local.eq_ignore_ascii_case(b"rtept") {
                    gpx_point = Some(gpx_point_from_start(&reader, &start)?);
                } else if local.eq_ignore_ascii_case(b"ele") && gpx_point.is_some() {
                    in_gpx_ele = true;
                } else if local.eq_ignore_ascii_case(b"coordinates") {
                    in_kml_coordinates = true;
                }
            }
            Event::Empty(start) => {
                let local = start.local_name();
                if local.as_ref().eq_ignore_ascii_case(b"trkpt")
                    || local.as_ref().eq_ignore_ascii_case(b"rtept")
                {
                    push_point(&mut points, gpx_point_from_start(&reader, &start)?)?;
                }
            }
            Event::Text(text) => {
                let value = text
                    .decode()
                    .map_err(|error| LocationRouteImportError::Xml(error.to_string()))?;
                if in_gpx_ele {
                    if let Some(point) = gpx_point.as_mut() {
                        point.altitude_mm = meters_to_mm(value.trim())?;
                    }
                } else if in_kml_coordinates {
                    parse_kml_coordinates(&value, &mut points)?;
                }
            }
            Event::CData(text) if in_kml_coordinates => {
                let value = text
                    .decode()
                    .map_err(|error| LocationRouteImportError::Xml(error.to_string()))?;
                parse_kml_coordinates(&value, &mut points)?;
            }
            Event::End(end) => {
                let local = end.local_name();
                let local = local.as_ref();
                if local.eq_ignore_ascii_case(b"trkpt") || local.eq_ignore_ascii_case(b"rtept") {
                    push_point(
                        &mut points,
                        gpx_point.take().ok_or_else(|| {
                            LocationRouteImportError::Xml(
                                "route point ended without a matching start".to_owned(),
                            )
                        })?,
                    )?;
                } else if local.eq_ignore_ascii_case(b"ele") {
                    in_gpx_ele = false;
                } else if local.eq_ignore_ascii_case(b"coordinates") {
                    in_kml_coordinates = false;
                }
            }
            Event::DocType(_) | Event::GeneralRef(_) => {
                return Err(LocationRouteImportError::XmlDeclaration);
            }
            Event::Eof => break,
            Event::CData(_) | Event::Decl(_) | Event::PI(_) | Event::Comment(_) => {}
        }
    }

    let Some(root) = root else {
        return Err(LocationRouteImportError::Format);
    };
    if !root.eq_ignore_ascii_case(b"gpx") && !root.eq_ignore_ascii_case(b"kml") {
        return Err(LocationRouteImportError::Format);
    }
    if !(2..=MAX_LOCATION_ROUTE_POINTS_V2).contains(&points.len()) {
        return Err(LocationRouteImportError::Points);
    }
    Ok(points)
}

fn gpx_point_from_start(
    reader: &Reader<&[u8]>,
    start: &BytesStart<'_>,
) -> Result<LocationV2, LocationRouteImportError> {
    let mut latitude = None;
    let mut longitude = None;
    for attribute in start.attributes().with_checks(true) {
        let attribute =
            attribute.map_err(|error| LocationRouteImportError::Xml(error.to_string()))?;
        let value = attribute
            .decode_and_unescape_value(reader.decoder())
            .map_err(|error| LocationRouteImportError::Xml(error.to_string()))?;
        match attribute.key {
            QName(b"lat") => latitude = Some(degrees_to_e7(value.trim(), 90)?),
            QName(b"lon") => longitude = Some(degrees_to_e7(value.trim(), 180)?),
            _ => {}
        }
    }
    Ok(LocationV2 {
        latitude_e7: latitude.ok_or(LocationRouteImportError::Points)?,
        longitude_e7: longitude.ok_or(LocationRouteImportError::Points)?,
        altitude_mm: 0,
        accuracy_mm: DEFAULT_ACCURACY_MM,
    })
}

fn parse_kml_coordinates(
    value: &str,
    points: &mut Vec<LocationV2>,
) -> Result<(), LocationRouteImportError> {
    for coordinate in value.split_ascii_whitespace() {
        let mut parts = coordinate.split(',');
        let longitude_e7 = degrees_to_e7(parts.next().unwrap_or_default(), 180)?;
        let latitude_e7 = degrees_to_e7(parts.next().unwrap_or_default(), 90)?;
        let altitude_mm = parts.next().map_or(Ok(0), meters_to_mm)?;
        if parts.next().is_some() {
            return Err(LocationRouteImportError::Points);
        }
        push_point(
            points,
            LocationV2 {
                latitude_e7,
                longitude_e7,
                altitude_mm,
                accuracy_mm: DEFAULT_ACCURACY_MM,
            },
        )?;
    }
    Ok(())
}

#[allow(clippy::cast_possible_truncation)]
fn degrees_to_e7(value: &str, maximum: i32) -> Result<i32, LocationRouteImportError> {
    let degrees = value
        .parse::<f64>()
        .map_err(|_| LocationRouteImportError::Points)?;
    if !degrees.is_finite() || degrees < -f64::from(maximum) || degrees > f64::from(maximum) {
        return Err(LocationRouteImportError::Points);
    }
    // The geodetic bounds above keep the rounded E7 value inside i32.
    Ok((degrees * 10_000_000.0).round() as i32)
}

#[allow(clippy::cast_possible_truncation)]
fn meters_to_mm(value: &str) -> Result<i32, LocationRouteImportError> {
    let meters = value
        .parse::<f64>()
        .map_err(|_| LocationRouteImportError::Points)?;
    if !meters.is_finite() || !(-1_000.0..=100_000.0).contains(&meters) {
        return Err(LocationRouteImportError::Points);
    }
    // The altitude bounds above keep the rounded millimeter value inside i32.
    Ok((meters * 1_000.0).round() as i32)
}

fn push_point(
    points: &mut Vec<LocationV2>,
    point: LocationV2,
) -> Result<(), LocationRouteImportError> {
    if points.len() >= MAX_LOCATION_ROUTE_POINTS_V2 || !point.is_valid() {
        return Err(LocationRouteImportError::Points);
    }
    points.push(point);
    Ok(())
}

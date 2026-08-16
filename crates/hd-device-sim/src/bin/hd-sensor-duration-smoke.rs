use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use hd_core::{InstanceActionV2, SensorInjectionV2, SensorPoseV2, sensor_motion_frame};
use hd_device_sim::{DeviceSimulatorV2, SensorOverrideRuntimeV2};
use serde_json::json;
use tokio::sync::Mutex;

#[tokio::main]
async fn main() -> Result<()> {
    let simulator = Arc::new(Mutex::new(DeviceSimulatorV2::default()));
    let runtime = SensorOverrideRuntimeV2::new(Arc::clone(&simulator));
    let supervisor = tokio::spawn(runtime.clone().run());

    check_duration_lifecycle(&runtime, &simulator).await?;
    let right_angle_frame = check_pose_lifecycle(&runtime, &simulator).await?;

    supervisor.abort();
    let _ = supervisor.await;
    let evidence = json!({
        "schema_version": 2,
        "timed_override_reset": true,
        "persistent_override": true,
        "stale_deadline_rejected": true,
        "independent_deadlines": true,
        "minimum_timed_duration_ms": 200,
        "single_supervisor": true,
        "pose_atomic_frame": true,
        "pose_aosp_matrix_order": "rz_ry_rx",
        "pose_right_angle_frame": right_angle_frame,
        "pose_gyro_settled": true
    });
    if let Some(path) = std::env::var_os("HD_SMOKE_OUTPUT").map(PathBuf::from) {
        ensure!(path.is_absolute(), "HD_SMOKE_OUTPUT must be absolute");
        hd_platform::write_owner_only(&path, &serde_json::to_vec_pretty(&evidence)?)?;
    }
    println!("{}", serde_json::to_string(&evidence)?);
    Ok(())
}

async fn check_duration_lifecycle(
    runtime: &SensorOverrideRuntimeV2,
    simulator: &Arc<Mutex<DeviceSimulatorV2>>,
) -> Result<()> {
    runtime
        .apply(sensor("accelerometer", &[1, 2, 3], 200))
        .await?;
    ensure!(
        simulator
            .lock()
            .await
            .state()
            .sensors
            .contains_key("accelerometer"),
        "timed override was not applied"
    );
    tokio::time::sleep(Duration::from_millis(320)).await;
    ensure!(
        !simulator
            .lock()
            .await
            .state()
            .sensors
            .contains_key("accelerometer"),
        "timed override did not return to the default sensor value"
    );

    runtime
        .apply(sensor("accelerometer", &[4, 5, 6], 300))
        .await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    runtime
        .apply(sensor("accelerometer", &[7, 8, 9], 0))
        .await?;
    tokio::time::sleep(Duration::from_millis(320)).await;
    let persistent = simulator
        .lock()
        .await
        .state()
        .sensors
        .get("accelerometer")
        .cloned()
        .context("old deadline removed the newer persistent override")?;
    ensure!(
        persistent.values_microunits == [7, 8, 9],
        "persistent override values changed after an older deadline"
    );

    runtime.apply(sensor("light", &[42_000_000], 200)).await?;
    runtime
        .apply(sensor("gyroscope", &[10, 20, 30], 400))
        .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    {
        let state = simulator.lock().await;
        ensure!(
            !state.state().sensors.contains_key("light")
                && state.state().sensors.contains_key("gyroscope"),
            "independent sensor deadlines were not respected"
        );
    }
    tokio::time::sleep(Duration::from_millis(220)).await;
    ensure!(
        !simulator
            .lock()
            .await
            .state()
            .sensors
            .contains_key("gyroscope"),
        "later independent sensor deadline did not expire"
    );

    ensure!(
        runtime
            .apply(sensor("proximity", &[2_500_000], 199))
            .await
            .is_err(),
        "sub-report-interval duration was accepted"
    );

    Ok(())
}

async fn check_pose_lifecycle(
    runtime: &SensorOverrideRuntimeV2,
    simulator: &Arc<Mutex<DeviceSimulatorV2>>,
) -> Result<hd_core::SensorMotionFrameV2> {
    let baseline = sensor_motion_frame(SensorPoseV2::default(), SensorPoseV2::default());
    ensure!(
        baseline.accelerometer_microunits == [0, 9_806_650, 0]
            && baseline.magnetometer_microunits == [0, 5_900_000, -48_400_000]
            && baseline.gyroscope_microunits == [0, 0, 0],
        "zero pose does not reproduce AOSP sensor baselines"
    );
    let right_angle_pose = SensorPoseV2 {
        x_millidegrees: 90_000,
        y_millidegrees: 0,
        z_millidegrees: 0,
        transition_ms: 200,
    };
    let revision_before_pose = simulator.lock().await.state().revision;
    let right_angle_frame = runtime.apply_pose(right_angle_pose).await?;
    ensure!(
        right_angle_frame.accelerometer_microunits == [0, 0, -9_806_650]
            && right_angle_frame.magnetometer_microunits == [0, -48_400_000, -5_900_000]
            && right_angle_frame.gyroscope_microunits == [7_853_982, 0, 0],
        "right-angle pose does not match the AOSP Rz*Ry*Rx model: {right_angle_frame:?}"
    );
    {
        let state = simulator.lock().await;
        ensure!(
            state.state().revision == revision_before_pose + 1
                && state.state().sensor_pose == Some(right_angle_pose)
                && ["accelerometer", "magnetometer", "gyroscope"]
                    .iter()
                    .all(|sensor| state.state().sensors.contains_key(*sensor)),
            "pose frame was not committed atomically"
        );
    }
    tokio::time::sleep(Duration::from_millis(320)).await;
    {
        let state = simulator.lock().await;
        ensure!(
            state.state().sensors.contains_key("accelerometer")
                && state.state().sensors.contains_key("magnetometer")
                && !state.state().sensors.contains_key("gyroscope"),
            "pose angular velocity did not settle independently"
        );
    }
    ensure!(
        InstanceActionV2::SetSensorPose {
            pose: SensorPoseV2 {
                x_millidegrees: 180_001,
                ..SensorPoseV2::default()
            }
        }
        .validate()
        .is_err()
            && InstanceActionV2::SetSensorPose {
                pose: SensorPoseV2 {
                    transition_ms: 199,
                    ..SensorPoseV2::default()
                }
            }
            .validate()
            .is_err(),
        "invalid pose angle or transition was accepted"
    );

    Ok(right_angle_frame)
}

fn sensor(name: &str, values: &[i64], duration_ms: u32) -> SensorInjectionV2 {
    SensorInjectionV2 {
        sensor: name.to_owned(),
        values_microunits: values.to_vec(),
        duration_ms,
    }
}

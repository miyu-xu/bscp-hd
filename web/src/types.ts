export interface DisplayConfig {
  width: number;
  height: number;
  dpi: number;
  refresh_rate_hz: number;
  orientation: 'portrait' | 'landscape' | 'reverse_portrait' | 'reverse_landscape';
  vsync: 'on' | 'off';
  show_host_fps: boolean;
  secondary_displays: SecondaryDisplayConfig[];
}

export interface SecondaryDisplayConfig {
  id: string;
  name: string;
  width: number;
  height: number;
  dpi: number;
  refresh_rate_hz: 30 | 60 | 90 | 120;
}

export type DisplayId = { kind: 'primary' } | { kind: 'secondary'; id: string };

export interface RuntimeDisplay {
  display_id: DisplayId;
  scanout_id: number;
  name: string;
  width: number;
  height: number;
  dpi: number;
  refresh_rate_hz: number;
}

export interface InstanceSpec {
  schema_version: number;
  id: string;
  name: string;
  guest_kind: 'android' | 'microdroid';
  microdroid: {
    debug_level: 'none' | 'full';
    cpu_topology: 'one_cpu' | 'match_host';
    payload:
      | { kind: 'empty' }
      | { kind: 'uploaded'; upload_id: string; sha256: string; config_path: 'assets/vm_config.json' };
    payload_extra_apk_count: number | null;
    extra_apks: Array<{ upload_id: string; sha256: string }>;
    encrypted_storage_mib: number | null;
  } | null;
  cpu_count: number;
  memory_mib: number;
  display: DisplayConfig;
  adb: { mode: 'disabled' | 'loopback'; host_port: number | null; executable: string | null };
  artifacts: { store_root: string; guest_bundle_digest: string; host_bundle_digest: string } | null;
  boot: { kernel_log_level: number; panic_timeout_seconds: number; boot_animation: boolean };
  devices: Record<string, boolean>;
  host_audio_input: 'disabled' | 'default_microphone';
  restart_policy: 'never' | 'on_failure';
  labels: Record<string, string>;
}

export interface InstanceStatus {
  observed: string;
  revision: number;
  last_error?: { code?: string; message?: string } | null;
}

export interface ScreenRecordingStatus {
  id: string;
  instance_id: string;
  display_id: DisplayId;
  max_duration_seconds: number;
  started_at: string;
}

export interface ScreenRecordingRecord {
  id: string;
  instance_id: string;
  display_id: DisplayId;
  path: string;
  sha256: string;
  size_bytes: number;
  duration_millis: number;
  started_at: string;
  finished_at: string;
}

export interface LocationRouteStatus {
  id: string;
  name: string;
  point_count: number;
  current_point: number;
  interval_ms: number;
  repeat: boolean;
  state: 'playing' | 'paused';
  started_at: string;
}

export interface LocationRouteRecord {
  id: string;
  name: string;
  point_count: number;
  applied_points: number;
  repeat: boolean;
  reason: 'completed' | 'stopped' | 'failed';
  error_code: string | null;
  started_at: string;
  finished_at: string;
}

export interface UwbRanging {
  distance_cm: number;
}

export interface ModemState {
  operator_numeric: string;
  operator_long_name: string;
  operator_short_name: string;
  signal_strength: number;
  registered: boolean;
}

export interface SensorPose {
  x_millidegrees: number;
  y_millidegrees: number;
  z_millidegrees: number;
  transition_ms: number;
}

export interface PowerwashBackup {
  id: string;
  reason: 'powerwash' | 'restore_rollback';
  source_revision: number;
  size_bytes: number;
  sha256: string;
  created_at: string;
}

export interface BluetoothPeerState {
  peer_id: string;
  name: string;
  kind: 'gatt' | 'beacon' | 'scripted_beacon' | 'hid_keyboard';
  advertising: boolean;
  scripted_frame_count?: number;
  repeat: boolean;
  keyboard_reports_sent: number;
}

export interface BluetoothHciCaptureRecord {
  capture_id: string;
  file_name: string;
  requested_duration_ms: number;
  packets_captured: number;
  packets_dropped: number;
  output_size_bytes: number;
  truncated: boolean;
  started_at: string;
  finished_at: string;
}

export interface InstanceSummary {
  id: string;
  name: string;
  guest_kind: 'android' | 'microdroid';
  status: InstanceStatus;
  adb_ready: boolean;
  frame_generation: number;
  host_fps_milli: number | null;
  screen_recording: ScreenRecordingStatus | null;
  last_screen_recording: ScreenRecordingRecord | null;
}

export interface InstanceRecord {
  spec: InstanceSpec;
  status: InstanceStatus;
  adb_serial: string | null;
  adb_ready: boolean;
  frame_generation: number;
  runtime_displays: RuntimeDisplay[];
  host_fps_milli: number | null;
  screen_recording: ScreenRecordingStatus | null;
  last_screen_recording: ScreenRecordingRecord | null;
  location_route: LocationRouteStatus | null;
  last_location_route: LocationRouteRecord | null;
  uwb_ranging: UwbRanging | null;
  modem_state: ModemState | null;
  sensor_pose: SensorPose | null;
  powerwash_backup: PowerwashBackup | null;
  storage_transaction: unknown | null;
  bluetooth_peers: BluetoothPeerState[];
  last_bluetooth_hci_capture: BluetoothHciCaptureRecord | null;
}

export interface DeviceCapability {
  id: string;
  backend: 'official_component' | 'simulated' | 'software_backed' | 'unsupported';
  available: boolean;
  boundary: string;
  features: string[];
  runtime: {
    probed: boolean;
    installed: boolean;
    configured: boolean;
    running: boolean;
    controllable: boolean;
    verified: boolean;
    detail: string;
  };
}

export interface ResourceCapability {
  id: 'host.resources';
  status: 'supported' | 'blocked' | 'unsupported';
  required: boolean;
  detail: string;
  properties: Record<string, string>;
}

export interface NetworkSetupState {
  supported: boolean;
  health: 'checking' | 'ready' | 'maintenance' | 'degraded' | 'offline' | 'unknown';
  service_action: 'none' | 'install' | 'upgrade' | 'repair' | 'manual_repair';
  network_usable: boolean;
  installed: boolean;
  package_match: boolean;
  loaded: boolean;
  pf_configured: boolean;
  egress: string;
  vpn_nat_required: boolean;
  socket_vmnet: boolean;
  nat: string;
  unsafe_paths: boolean;
  detail: string;
}

export interface TitlebarPlayerState {
  android_selected: boolean;
  controls_visible: boolean;
  sidebar_visible: boolean;
  power_enabled: boolean;
  actions_enabled: boolean;
  recording_supported: boolean;
  recording_active: boolean;
  recording_enabled: boolean;
  show_host_fps: boolean;
  host_fps_milli: number;
}

export interface Snapshot {
  summaries: InstanceSummary[];
  selected: InstanceRecord | null;
  selected_display: DisplayId;
  host_runtime_current: boolean;
  screen_recording_supported: boolean;
  microdroid_supported: boolean;
  titlebar: TitlebarPlayerState;
  status: string;
  artifact_hint: string | null;
  diagnostic_artifact: string | null;
  android_bugreport_artifact: string | null;
  network_setup: NetworkSetupState;
  resource_capability: ResourceCapability | null;
  resource_capability_loading: boolean;
  device_capabilities: DeviceCapability[];
  device_capabilities_loading: boolean;
}

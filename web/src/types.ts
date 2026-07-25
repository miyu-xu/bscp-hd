export interface DisplayConfig {
  width: number;
  height: number;
  dpi: number;
  refresh_rate_hz: number;
  orientation: 'portrait' | 'landscape' | 'reverse_portrait' | 'reverse_landscape';
  vsync: 'on' | 'off';
  show_host_fps: boolean;
}

export interface InstanceSpec {
  schema_version: number;
  id: string;
  name: string;
  cpu_count: number;
  memory_mib: number;
  display: DisplayConfig;
  adb: { mode: 'disabled' | 'loopback'; host_port: number | null; executable: string | null };
  artifacts: { store_root: string; guest_bundle_digest: string; host_bundle_digest: string } | null;
  boot: { kernel_log_level: number; panic_timeout_seconds: number; boot_animation: boolean };
  devices: Record<string, boolean>;
  restart_policy: 'never' | 'on_failure';
  labels: Record<string, string>;
}

export interface InstanceStatus {
  observed: string;
  revision: number;
  last_error?: { code?: string; message?: string } | null;
}

export interface InstanceSummary {
  id: string;
  name: string;
  status: InstanceStatus;
  frame_generation: number;
  host_fps_milli: number | null;
}

export interface InstanceRecord {
  spec: InstanceSpec;
  status: InstanceStatus;
  adb_serial: string | null;
  frame_generation: number;
  host_fps_milli: number | null;
}

export interface Snapshot {
  summaries: InstanceSummary[];
  selected: InstanceRecord | null;
  status: string;
  artifact_hint: string | null;
}

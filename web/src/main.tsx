import React, { useEffect, useMemo, useState } from 'react';
import { createRoot } from 'react-dom/client';
import {
  Activity, AlertCircle, AppWindow, ArrowLeft, Battery, Bluetooth, Boxes, Camera,
  Check, ChevronDown, CirclePause, Clipboard, FileArchive, FolderOpen, Gauge,
    HardDrive, Home, MapPin, Minus, MonitorCog, MoreHorizontal, Nfc,
    PackagePlus, PanelLeftClose, PanelLeftOpen, Play, Plus, Power, Radio, RefreshCw,
    RotateCcw, RotateCw, Search, Settings, Signal, Square, SquareStack,
  SlidersHorizontal, Trash2, Volume1, Volume2, Wifi, X,
} from 'lucide-react';
import type { LucideIcon } from 'lucide-react';
import { post, type HostMessage } from './bridge';
import type { DeviceCapability, InstanceRecord, InstanceSpec, InstanceSummary, NetworkSetupState, ResourceCapability, Snapshot } from './types';
import './style.css';

type Page = 'player' | 'settings' | 'devices' | 'diagnostics';
type IconType = LucideIcon;

function keepGuestFocusOnTitlebarPress(event: React.SyntheticEvent<HTMLElement>) {
  // The Android render/input HWND owns focus while the Player is interactive. A WebView button's
  // default mousedown focuses the WebView first, then the host command immediately focuses crosvm
  // again. Avoid that cross-HWND focus round trip: it makes WebView2/DWM compose an intermediate
  // titlebar/guest frame and is visible as a flash while clicking navigation controls.
  event.preventDefault();
  event.stopPropagation();
}

const emptySnapshot: Snapshot = {
  summaries: [],
  selected: null,
  selected_display: { kind: 'primary' },
  host_runtime_current: true,
  screen_recording_supported: true,
  microdroid_supported: false,
  titlebar: {
    android_selected: false,
    controls_visible: false,
    sidebar_visible: false,
    power_enabled: false,
    actions_enabled: false,
    recording_supported: false,
    recording_active: false,
    recording_enabled: false,
    show_host_fps: false,
    host_fps_milli: 0,
  },
  status: '正在连接 HD Host…',
  artifact_hint: null,
  diagnostic_artifact: null,
  android_bugreport_artifact: null,
  network_setup: {
    supported: false,
    health: 'checking',
    service_action: 'none',
    network_usable: false,
    installed: false,
    package_match: false,
    loaded: false,
    pf_configured: false,
    egress: 'unknown',
    vpn_nat_required: false,
    socket_vmnet: false,
    nat: 'unknown',
    unsafe_paths: false,
    detail: '正在读取 Host 网络能力…',
  },
  resource_capability: null,
  resource_capability_loading: false,
  device_capabilities: [],
  device_capabilities_loading: false,
};

const activeStates = new Set([
  'preparing', 'starting_worker', 'launching_guest', 'negotiating_display',
  'guest_booting', 'adb_connecting', 'ready', 'pausing', 'paused', 'resuming',
  'recovering', 'stopping',
]);

const transitionStates = new Set([
  'preparing', 'starting_worker', 'launching_guest', 'negotiating_display',
  'guest_booting', 'adb_connecting', 'pausing', 'resuming', 'recovering',
  'stopping', 'deleting',
]);

function isActive(record: InstanceRecord | null) {
  return Boolean(record && activeStates.has(record.status.observed));
}

function requiresInstanceRestart(current: InstanceSpec, next: InstanceSpec) {
  if (current.guest_kind !== next.guest_kind) return true;
  if (current.guest_kind === 'microdroid') {
    return current.cpu_count !== next.cpu_count
      || current.memory_mib !== next.memory_mib
      || JSON.stringify(current.microdroid) !== JSON.stringify(next.microdroid)
      || JSON.stringify(current.adb) !== JSON.stringify(next.adb)
      || JSON.stringify(current.artifacts) !== JSON.stringify(next.artifacts);
  }
  const currentDisplay = { ...current.display, show_host_fps: false };
  const nextDisplay = { ...next.display, show_host_fps: false };
  return current.cpu_count !== next.cpu_count
    || current.memory_mib !== next.memory_mib
    || JSON.stringify(currentDisplay) !== JSON.stringify(nextDisplay)
    || JSON.stringify(current.adb) !== JSON.stringify(next.adb)
    || JSON.stringify(current.artifacts) !== JSON.stringify(next.artifacts)
    || JSON.stringify(current.boot) !== JSON.stringify(next.boot)
    || JSON.stringify(current.devices) !== JSON.stringify(next.devices)
    || current.host_audio_input !== next.host_audio_input;
}

function androidActionsReady(record: InstanceRecord | null) {
  return Boolean(record && record.status.observed === 'ready' && record.adb_ready);
}

function TitlebarButton({
  label,
  icon: Icon,
  onClick,
  disabled = false,
  active = false,
}: {
  label: string;
  icon: IconType;
  onClick: () => void;
  disabled?: boolean;
  active?: boolean;
}) {
  return (
    <button
      type="button"
      className={`icon-button${active ? ' active' : ''}`}
      aria-label={label}
      title={label}
      disabled={disabled}
      onPointerDown={keepGuestFocusOnTitlebarPress}
      onMouseDown={keepGuestFocusOnTitlebarPress}
      onDoubleClick={event => event.stopPropagation()}
      onClick={onClick}
    >
      <Icon size={16} strokeWidth={1.8} />
    </button>
  );
}

function PlayerTitlebarTools({ snapshot, busy }: { snapshot: Snapshot; busy: boolean }) {
  const state = snapshot.titlebar;
  if (!state.controls_visible) return null;
  const actionDisabled = busy || !state.actions_enabled;
  return (
    <div className="tool-group" aria-label="Android 控制">
      <TitlebarButton label="电源" icon={Power} disabled={busy || !state.power_enabled} onClick={() => post({ command: 'key', key: 'power' })} />
      <TitlebarButton label="音量减" icon={Volume1} disabled={actionDisabled} onClick={() => post({ command: 'key', key: 'volume_down' })} />
      <TitlebarButton label="音量加" icon={Volume2} disabled={actionDisabled} onClick={() => post({ command: 'key', key: 'volume_up' })} />
      <TitlebarButton label="旋转" icon={RotateCw} disabled={actionDisabled} onClick={() => post({ command: 'rotate' })} />
      <TitlebarButton label="截图" icon={Camera} disabled={actionDisabled} onClick={() => post({ command: 'screenshot' })} />
      <TitlebarButton
        label={state.recording_active ? '停止录屏' : '开始录屏'}
        icon={CirclePause}
        active={state.recording_active}
        disabled={busy || !state.recording_enabled}
        onClick={() => post({ command: state.recording_active ? 'stop_screen_recording' : 'start_screen_recording' })}
      />
      <TitlebarButton label="安装 APK" icon={PackagePlus} disabled={actionDisabled} onClick={() => post({ command: 'choose_install_apk' })} />
      <span className="tool-divider" />
      <TitlebarButton label="最近任务" icon={SquareStack} disabled={actionDisabled} onClick={() => post({ command: 'key', key: 'recent' })} />
      <TitlebarButton label="主页" icon={Home} disabled={actionDisabled} onClick={() => post({ command: 'key', key: 'home' })} />
      <TitlebarButton label="返回" icon={ArrowLeft} disabled={actionDisabled} onClick={() => post({ command: 'key', key: 'back' })} />
      {state.show_host_fps && <span className="titlebar-fps">{(state.host_fps_milli / 1000).toFixed(1)} FPS</span>}
    </div>
  );
}

function TopSurface() {
  const { snapshot, layout, busy } = useHostState();
  const sidebarTitle = layout.sidebar_visible ? '关闭侧边栏' : '打开侧边栏';
  return (
    <header className="titlebar surface-top" onDoubleClick={() => post({ command: 'window', action: 'maximize' })}>
        <button
          type="button"
          className="sidebar-toggle"
          aria-label={sidebarTitle}
          title={sidebarTitle}
          onPointerDown={keepGuestFocusOnTitlebarPress}
          onMouseDown={keepGuestFocusOnTitlebarPress}
          onDoubleClick={event => event.stopPropagation()}
          onClick={() => post({ command: 'toggle_sidebar' })}
        >
          {layout.sidebar_visible ? <PanelLeftClose size={16} /> : <PanelLeftOpen size={16} />}
        </button>
        <div
          className="drag-region"
          aria-label="拖动窗口"
          onPointerDown={event => {
            keepGuestFocusOnTitlebarPress(event);
            if (event.button === 0) post({ command: 'window', action: 'drag' });
          }}
        />
        {layout.page === 'player' && <PlayerTitlebarTools snapshot={snapshot} busy={busy} />}
        <div className="window-controls">
          <button type="button" aria-label="最小化" title="最小化" onPointerDown={keepGuestFocusOnTitlebarPress} onMouseDown={keepGuestFocusOnTitlebarPress} onDoubleClick={event => event.stopPropagation()} onClick={() => post({ command: 'window', action: 'minimize' })}><Minus size={14} /></button>
          <button type="button" aria-label="最大化或还原" title="最大化或还原" onPointerDown={keepGuestFocusOnTitlebarPress} onMouseDown={keepGuestFocusOnTitlebarPress} onDoubleClick={event => event.stopPropagation()} onClick={() => post({ command: 'window', action: 'maximize' })}><Square size={12} /></button>
          <button type="button" className="close" aria-label="关闭 HD" title="关闭 HD，实例继续运行" onPointerDown={keepGuestFocusOnTitlebarPress} onMouseDown={keepGuestFocusOnTitlebarPress} onDoubleClick={event => event.stopPropagation()} onClick={() => post({ command: 'window', action: 'close' })}><X size={15} /></button>
        </div>
    </header>
  );
}

async function copyText(value: string) {
  try {
    await navigator.clipboard.writeText(value);
    return true;
  } catch {
    const textarea = document.createElement('textarea');
    textarea.value = value;
    textarea.setAttribute('readonly', '');
    textarea.style.position = 'fixed';
    textarea.style.opacity = '0';
    document.body.appendChild(textarea);
    textarea.select();
    const copied = document.execCommand('copy');
    textarea.remove();
    return copied;
  }
}

function stateLabel(state?: string) {
  const labels: Record<string, string> = {
    defined: '未启动',
    preparing: '准备中',
    starting_worker: '启动 Worker',
    launching_guest: '启动 Guest',
    negotiating_display: '连接显示',
    guest_booting: 'Android 启动中',
    adb_connecting: '连接 ADB',
    ready: '运行中',
    pausing: '暂停中',
    paused: '已暂停',
    resuming: '恢复中',
    recovering: '恢复中',
    stopping: '停止中',
    stopped: '已停止',
    blocked: '已阻止',
    failed: '失败',
    deleting: '删除中',
    deleted: '已删除',
  };
  return labels[state ?? ''] ?? state ?? '未知';
}

function NoticeBanner({ message, busy, compact = false }: {
  message: string;
  busy: boolean;
  compact?: boolean;
}) {
  const [dismissed, setDismissed] = useState(false);
  const error = /失败|错误|无效|拒绝|不存在|不可|超时|not ready|not found|invalid/i.test(message);
  useEffect(() => {
    setDismissed(false);
    if (!message || busy) return undefined;
    const timer = window.setTimeout(() => setDismissed(true), error ? 8000 : 4000);
    return () => window.clearTimeout(timer);
  }, [message, busy, error]);
  if ((!message && !busy) || dismissed) return null;
  return (
    <div
      className={`notice-banner ${compact ? 'compact' : ''} ${error ? 'error' : ''}`}
      role={error ? 'alert' : 'status'}
      aria-live={error ? 'assertive' : 'polite'}
    >
      {busy ? <span className="spinner" /> : error ? <AlertCircle size={15} /> : <Check size={15} />}
      <span>{message || '正在处理…'}</span>
      {!compact && (
        <button type="button" aria-label="关闭提示" onClick={() => setDismissed(true)}>
          <X size={14} />
        </button>
      )}
    </div>
  );
}

function ConfirmDialog({
  open,
  title,
  description,
  confirmLabel,
  requiredText,
  danger = false,
  busy = false,
  onCancel,
  onConfirm,
}: {
  open: boolean;
  title: string;
  description: string;
  confirmLabel: string;
  requiredText?: string;
  danger?: boolean;
  busy?: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const [enteredText, setEnteredText] = useState('');
  useEffect(() => {
    setEnteredText('');
  }, [open, requiredText]);
  useEffect(() => {
    if (!open) return undefined;
    const handleKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape' && !busy) onCancel();
    };
    window.addEventListener('keydown', handleKey);
    return () => window.removeEventListener('keydown', handleKey);
  }, [open, busy, onCancel]);
  if (!open) return null;
  return (
    <div className="dialog-backdrop" role="presentation" onMouseDown={() => !busy && onCancel()}>
      <div
        className="confirm-dialog"
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="confirm-title"
        aria-describedby="confirm-description"
        onMouseDown={event => event.stopPropagation()}
      >
        <h2 id="confirm-title">{title}</h2>
        <p id="confirm-description">{description}</p>
        {requiredText !== undefined && (
          <label className="confirmation-text">
            <span>输入实例名 <strong>{requiredText}</strong> 继续</span>
            <input
              type="text"
              autoFocus
              autoComplete="off"
              spellCheck={false}
              value={enteredText}
              disabled={busy}
              onChange={event => setEnteredText(event.target.value)}
            />
          </label>
        )}
        <div className="dialog-actions">
          <button type="button" disabled={busy} onClick={onCancel}>取消</button>
          <button
            type="button"
            className={danger ? 'danger-button' : 'primary-button'}
            disabled={busy || (requiredText !== undefined && enteredText !== requiredText)}
            onClick={onConfirm}
          >
            {busy ? '正在处理…' : confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}

function InstanceRow({
  item,
  selected,
  onSelect,
  onMenu,
}: {
  item: InstanceSummary;
  selected: boolean;
  onSelect: () => void;
  onMenu: (event: React.MouseEvent<HTMLElement>) => void;
}) {
  const active = ['ready', 'paused'].includes(item.status.observed);
  return (
    <div
      className={`instance-row ${selected ? 'selected' : ''}`}
      onContextMenu={onMenu}
    >
      <button type="button" className="instance-row-main" onClick={onSelect}>
        <span className={`state-dot ${active ? 'active' : ''}`} />
        <span className="instance-copy">
          <span className="instance-name">{item.name}</span>
          <span className="instance-meta">
            {item.guest_kind === 'microdroid' ? 'Microdroid' : 'Android'} · {stateLabel(item.status.observed)}
            {item.status.observed === 'ready' && !item.adb_ready ? ' · ADB 连接中' : ''}
          </span>
        </span>
      </button>
      <button
        type="button"
        className="instance-menu-button"
        aria-label={`${item.name} 操作菜单`}
        onClick={onMenu}
      >
        <MoreHorizontal size={17} strokeWidth={1.8} />
      </button>
    </div>
  );
}

type SettingSectionId = 'general' | 'performance' | 'display' | 'adb' | 'payload' | 'artifacts' | 'devices' | 'advanced';

const androidSettingSections: ReadonlyArray<readonly [SettingSectionId, string, IconType]> = [
  ['general', '常规', Settings],
  ['performance', '性能', Gauge],
  ['display', '显示', MonitorCog],
  ['adb', 'Android / ADB', AppWindow],
  ['artifacts', '制品与启动', HardDrive],
  ['devices', '设备模拟', Boxes],
  ['advanced', '高级', SlidersHorizontal],
];

const microdroidSettingSections: ReadonlyArray<readonly [SettingSectionId, string, IconType]> = [
  ['general', '常规', Settings],
  ['performance', '性能', Gauge],
  ['payload', '工作负载', FileArchive],
  ['adb', '调试与 ADB', AppWindow],
  ['advanced', '高级', SlidersHorizontal],
];

const deviceLabels: Record<string, string> = {
  audio: '音频',
  power: '电池与电源',
  bluetooth: 'Bluetooth',
  camera: '摄像头',
  gnss: '定位',
  network: '网络模拟',
  nfc: 'NFC',
  sensors: '传感器',
  modem: '蜂窝网络',
  uwb: 'UWB',
};

const deviceBackendLabels: Record<DeviceCapability['backend'], string> = {
  official_component: '官方组件',
  simulated: '模拟后端',
  software_backed: '软件后端',
  unsupported: '不支持',
};

const displayPresets = [
  { label: '手机 · 720 × 1280', width: 720, height: 1280, dpi: 320, orientation: 'portrait' as const },
  { label: '手机 · 1080 × 1920', width: 1080, height: 1920, dpi: 420, orientation: 'portrait' as const },
  { label: '高分屏 · 1440 × 2560', width: 1440, height: 2560, dpi: 560, orientation: 'portrait' as const },
  { label: '平板横屏 · 1280 × 720', width: 1280, height: 720, dpi: 320, orientation: 'landscape' as const },
];

function editableSpec(record: InstanceRecord | null, unsupportedDevices: string[]) {
  if (!record) return null;
  const spec = structuredClone(record.spec);
  // Product input is the real pointer chain on the Android render surface. Older development
  // instances may still carry this compatibility field, but the release UI must not expose a
  // second, competing touchpad model.
  spec.devices.touchpad = false;
  unsupportedDevices.forEach(key => {
    if (key in spec.devices) spec.devices[key] = false;
  });
  spec.host_audio_input ??= 'disabled';
  if (!spec.devices.audio) spec.host_audio_input = 'disabled';
  return spec;
}

function NumberField({
  label,
  value,
  onChange,
  min,
  max,
  suffix,
  error,
}: {
  label: string;
  value: number;
  onChange: (value: number) => void;
  min: number;
  max: number;
  suffix?: string;
  error?: string;
}) {
  return (
    <label className={`setting-row ${error ? 'invalid' : ''}`}>
      <span>{label}</span>
      <span className="field-column">
        <span className="field-with-suffix">
          <input
            type="number"
            value={Number.isFinite(value) ? value : ''}
            min={min}
            max={max}
            aria-invalid={Boolean(error)}
            onChange={event => onChange(event.target.value === '' ? Number.NaN : Number(event.target.value))}
          />
          {suffix && <span>{suffix}</span>}
        </span>
        {error && <span className="field-error">{error}</span>}
      </span>
    </label>
  );
}

function numericProperty(capability: ResourceCapability | null, key: string) {
  const value = capability?.properties[key];
  if (!value || !/^\d+$/.test(value)) return null;
  const parsed = Number(value);
  return Number.isSafeInteger(parsed) ? parsed : null;
}

function formatBytes(value: number | null) {
  if (value === null) return '—';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let scaled = value;
  let unit = 0;
  while (scaled >= 1024 && unit < units.length - 1) {
    scaled /= 1024;
    unit += 1;
  }
  const digits = scaled >= 10 || Number.isInteger(scaled) ? 0 : 1;
  return `${scaled.toFixed(digits)} ${units[unit]}`;
}

function ResourceAdmissionCard({
  capability,
  loading,
}: {
  capability: ResourceCapability | null;
  loading: boolean;
}) {
  const requestedMemory = numericProperty(capability, 'requested_memory_bytes');
  const hostReserve = numericProperty(capability, 'host_memory_reserve_bytes');
  const requiredMemory = requestedMemory === null || hostReserve === null
    ? null
    : requestedMemory + hostReserve;
  const availableMemory = numericProperty(capability, 'available_memory_bytes');
  const requiredDisk = numericProperty(capability, 'required_disk_bytes');
  const availableDisk = numericProperty(capability, 'available_disk_bytes');
  const mode = capability?.properties.disk_requirement_mode;
  const status = loading
    ? '正在检测'
    : capability?.status === 'supported'
      ? '准入通过'
      : capability?.status === 'blocked'
        ? '资源不足'
        : capability
          ? '无法检测'
          : '尚未检测';
  const statusClass = capability?.status === 'supported' && !loading ? 'available' : capability?.status === 'blocked' ? 'blocked' : '';
  return (
    <article className={`resource-admission-card ${capability?.status === 'blocked' ? 'blocked' : ''}`} role={capability?.status === 'blocked' ? 'alert' : undefined}>
      <header>
        <span><HardDrive size={16} />宿主资源准入</span>
        <span className={`capability-state ${statusClass}`}>{status}</span>
      </header>
      <dl>
        <div><dt>处理器</dt><dd>需要 {capability?.properties.requested_cpus ?? '—'} 核 · 当前可用 {capability?.properties.available_lease_cpus ?? capability?.properties.guest_cpu_capacity ?? '—'} 核{capability?.properties.exclusive_cpu_lease === 'true' ? ' · 独占' : ''}</dd></div>
        <div><dt>内存预算</dt><dd>需要 {formatBytes(requiredMemory)} · 可用 {formatBytes(availableMemory)}</dd></div>
        <div><dt>磁盘空间</dt><dd>需要 {formatBytes(requiredDisk)} · 可用 {formatBytes(availableDisk)}</dd></div>
        <div><dt>存储模式</dt><dd>{mode === 'existing_instance_storage' ? '已有实例 · 仅保留运行余量' : mode === 'new_instance_storage' ? '新建存储 · 预留完整容量' : '—'}</dd></div>
      </dl>
      <p>{loading ? '正在读取当前实例的 CPU、内存和磁盘准入状态…' : capability?.detail ?? 'Host 尚未返回当前实例的资源准入结果。'}</p>
      <footer>
        <small>显示已保存配置的最近一次检查；真正启动时 Host 会重新原子判定。</small>
        <button type="button" className="secondary-button" disabled={loading} onClick={() => post({ command: 'resource_admission_refresh' })}>{loading ? '检测中…' : '重新检测'}</button>
      </footer>
    </article>
  );
}

function validateSpec(spec: InstanceSpec) {
  const errors: Record<string, string> = {};
  const microdroid = spec.guest_kind === 'microdroid';
  if (!spec.name.trim()) errors.name = '实例名称不能为空';
  if (!Number.isInteger(spec.cpu_count) || spec.cpu_count < 1 || spec.cpu_count > (microdroid ? 1 : 256)) {
    errors.cpu = microdroid ? '当前 Microdroid 运行时仅支持 1 个 vCPU' : '请输入 1–256';
  }
  const minimumMemory = microdroid ? 256 : 2048;
  if (!Number.isInteger(spec.memory_mib) || spec.memory_mib < minimumMemory || spec.memory_mib > 1048576) errors.memory = `请输入 ${minimumMemory}–1048576 MiB`;
  if (!microdroid) {
    if (!Number.isInteger(spec.display.width) || spec.display.width < 320 || spec.display.width > 8192) errors.width = '请输入 320–8192';
    if (!Number.isInteger(spec.display.height) || spec.display.height < 320 || spec.display.height > 8192) errors.height = '请输入 320–8192';
    if (!Number.isInteger(spec.display.dpi) || spec.display.dpi < 72 || spec.display.dpi > 960) errors.dpi = '请输入 72–960';
    if (spec.display.secondary_displays.length > 3
      || spec.display.secondary_displays.some(display => !display.name.trim()
        || !Number.isInteger(display.width) || display.width < 320 || display.width > 8192
        || !Number.isInteger(display.height) || display.height < 320 || display.height > 8192
        || !Number.isInteger(display.dpi) || display.dpi < 72 || display.dpi > 960)) {
      errors.secondaryDisplays = '副显示器最多 3 个；名称不能为空，宽高需为 320–8192，DPI 需为 72–960';
    }
    if (!Number.isInteger(spec.boot.kernel_log_level) || spec.boot.kernel_log_level < 0 || spec.boot.kernel_log_level > 7) errors.logLevel = '请输入 0–7';
    if (!Number.isInteger(spec.boot.panic_timeout_seconds) || spec.boot.panic_timeout_seconds < 0 || spec.boot.panic_timeout_seconds > 300) errors.panic = '请输入 0–300 秒';
  } else if (!spec.microdroid) {
    errors.payload = '缺少 Microdroid 工作负载配置';
  } else {
    if (!['one_cpu', 'match_host'].includes(spec.microdroid.cpu_topology)) {
      errors.cpu = '请选择单 vCPU 或匹配宿主';
    }
    if (spec.microdroid.encrypted_storage_mib !== null
      && (!Number.isInteger(spec.microdroid.encrypted_storage_mib)
        || spec.microdroid.encrypted_storage_mib < 10
        || spec.microdroid.encrypted_storage_mib > 4096)) {
      errors.storage = '请输入 10–4096 MiB';
    }
    if (spec.microdroid.debug_level === 'none' && spec.adb.mode !== 'disabled') {
      errors.adb = 'None 调试模式必须禁用 ADB';
    }
    if (spec.microdroid.extra_apks.length > 8) errors.extraApks = '额外 APK 最多 8 个';
    if (spec.microdroid.payload_extra_apk_count !== null
      && spec.microdroid.extra_apks.length !== spec.microdroid.payload_extra_apk_count) {
      errors.extraApks = `主 Payload 声明 ${spec.microdroid.payload_extra_apk_count} 个额外 APK，当前已选择 ${spec.microdroid.extra_apks.length} 个`;
    }
    if (spec.microdroid.payload.kind === 'empty' && spec.microdroid.extra_apks.length > 0) {
      errors.extraApks = '额外 APK 需要先选择主 Payload APK';
    }
    if (spec.artifacts) errors.artifacts = 'Microdroid 制品由签名运行包提供，不能使用 Android 制品选择';
  }
  if (spec.adb.mode === 'disabled' && spec.adb.host_port !== null) errors.adb = '禁用 ADB 时不能保留端口';
  if (spec.artifacts) {
    if (!spec.artifacts.store_root.trim()) errors.artifactRoot = '请输入制品仓库路径';
    if (!/^[0-9a-f]{64}$/i.test(spec.artifacts.guest_bundle_digest)) errors.guestDigest = '需要 64 位十六进制 digest';
    if (!/^[0-9a-f]{64}$/i.test(spec.artifacts.host_bundle_digest)) errors.hostDigest = '需要 64 位十六进制 digest';
  }
  return errors;
}

function SettingsPage({
  record,
  busy,
  apkPath,
  setApkPath,
  onChooseApk,
  onInstallApk,
  onChoosePayload,
  onSave,
  deviceCapabilities,
  capabilitiesLoading,
  resourceCapability,
  resourceCapabilityLoading,
}: {
  record: InstanceRecord | null;
  busy: boolean;
  apkPath: string;
  setApkPath: (path: string) => void;
  onChooseApk: () => void;
  onInstallApk: () => void;
  onChoosePayload: () => void;
  onSave: (spec: InstanceSpec, restart: boolean, expectedRevision: number) => void;
  deviceCapabilities: DeviceCapability[];
  capabilitiesLoading: boolean;
  resourceCapability: ResourceCapability | null;
  resourceCapabilityLoading: boolean;
}) {
  const capabilityMap = useMemo(
    () => new Map(deviceCapabilities.map(capability => [capability.id, capability])),
    [deviceCapabilities],
  );
  const unsupportedDevices = useMemo(
    () => deviceCapabilities.filter(capability => !capability.available).map(capability => capability.id),
    [deviceCapabilities],
  );
  const [section, setSection] = useState<SettingSectionId>('general');
  const unsupportedKey = unsupportedDevices.join('\0');
  const [draft, setDraft] = useState<InstanceSpec | null>(() => editableSpec(record, unsupportedDevices));
  const [confirmRestart, setConfirmRestart] = useState(false);
  const specKey = record ? JSON.stringify(record.spec) : '';

  useEffect(() => {
    setDraft(editableSpec(record, unsupportedDevices));
    setConfirmRestart(false);
  }, [record?.spec.id, specKey, unsupportedKey]);

  const settingsDirty = Boolean(record && draft && JSON.stringify(draft) !== JSON.stringify(record.spec));
  useEffect(() => {
    if (!record) return undefined;
    post({
      command: 'settings_dirty',
      instance_id: record.spec.id,
      revision: record.status.revision,
      dirty: settingsDirty,
    });
    return () => post({
      command: 'settings_dirty',
      instance_id: record.spec.id,
      revision: record.status.revision,
      dirty: false,
    });
  }, [settingsDirty, record?.spec.id, record?.status.revision]);

  useEffect(() => {
    if (section === 'devices' && record?.spec.guest_kind === 'android') {
      post({ command: 'device_capabilities_refresh' });
    }
  }, [section, record?.spec.id, record?.spec.guest_kind]);

  if (!record || !draft) {
    return <section className="empty-page"><h1>设置</h1><p>请先选择一个实例。</p></section>;
  }

  const mutate = (fn: (next: InstanceSpec) => void) => {
    const next = structuredClone(draft);
    fn(next);
    setDraft(next);
  };
  const errors = validateSpec(draft);
  const dirty = settingsDirty;
  const isMicrodroid = draft.guest_kind === 'microdroid';
  const settingSections = isMicrodroid ? microdroidSettingSections : androidSettingSections;
  const active = isActive(record);
  const valid = Object.keys(errors).length === 0;
  const restartRequired = active && requiresInstanceRestart(record.spec, draft);
  const restartSafe = !restartRequired || record.adb_ready;
  const canSave = dirty && valid && !busy && restartSafe;
  const save = () => {
    if (!canSave) return;
    if (restartRequired) setConfirmRestart(true);
    else onSave(draft, false, record.status.revision);
  };
  const applyPreset = (index: number) => {
    const preset = displayPresets[index];
    if (!preset) return;
    mutate(next => {
      next.display.width = preset.width;
      next.display.height = preset.height;
      next.display.dpi = preset.dpi;
      next.display.orientation = preset.orientation;
    });
  };

  return (
    <section className="settings-page">
      <header className="content-header">
        <div className="content-title"><Settings size={19} /><span>{isMicrodroid ? 'Microdroid 设置' : 'Android 设置'}</span></div>
        <div className="setting-action-row">
          <button type="button" className="secondary-button" disabled={!dirty || busy} onClick={() => setDraft(editableSpec(record, unsupportedDevices))}>放弃更改</button>
          <button
            type="button"
            className="primary-button"
            disabled={!canSave}
            title={!restartSafe ? 'ADB 未就绪；为保护实例数据，运行中不能保存并重启' : undefined}
            onClick={save}
          >
            {busy ? '正在保存…' : restartRequired ? '保存并重启' : '保存更改'}
          </button>
        </div>
      </header>
      <div className="settings-body">
        <nav className="settings-nav" aria-label="设置分类">
          {settingSections.map(([id, label, Icon]) => (
            <button
              type="button"
              key={id}
              className={section === id ? 'active' : ''}
              onClick={() => setSection(id)}
            >
              <Icon size={18} strokeWidth={1.8} /><span>{label}</span>
            </button>
          ))}
        </nav>
        <div className="settings-content">
          {restartRequired && !record.adb_ready && (
            <div className="inline-callout warning">
              {isMicrodroid
                ? '当前 Payload 没有可用的 ADB 关机通道。请先从实例菜单明确执行强制停止，再保存设置。'
                : 'ADB 控制尚未就绪。为避免强制关机损坏实例数据，运行中不能保存并重启。'}
            </div>
          )}
          {dirty && restartRequired && record.adb_ready && (
            <div className="inline-callout warning">
              当前实例正在运行。保存后 HD 将安全停止并重新启动 {isMicrodroid ? 'Microdroid' : 'Android'}。
            </div>
          )}
          {section === 'general' && (
            <>
              <h2>常规</h2>
              <label className={`setting-row ${errors.name ? 'invalid' : ''}`}>
                <span>实例名称</span>
                <span className="field-column">
                  <input
                    value={draft.name}
                    aria-invalid={Boolean(errors.name)}
                    onChange={event => mutate(next => { next.name = event.target.value; })}
                  />
                  {errors.name && <span className="field-error">{errors.name}</span>}
                </span>
              </label>
              <label className="setting-row">
                <span>异常后自动重启</span>
                <input
                  type="checkbox"
                  checked={draft.restart_policy === 'on_failure'}
                  onChange={event => mutate(next => { next.restart_policy = event.target.checked ? 'on_failure' : 'never'; })}
                />
              </label>
            </>
          )}
          {section === 'performance' && (
            <>
              <h2>性能</h2>
              {isMicrodroid && draft.microdroid ? (
                <label className={`setting-row ${errors.cpu ? 'invalid' : ''}`}>
                  <span>CPU 拓扑</span>
                  <span className="field-column">
                    <select
                      value={draft.microdroid.cpu_topology}
                      aria-invalid={Boolean(errors.cpu)}
                      onChange={event => mutate(next => {
                        if (next.microdroid) next.microdroid.cpu_topology = event.target.value as 'one_cpu' | 'match_host';
                      })}
                    >
                      <option value="one_cpu">单 vCPU（可并行运行实例）</option>
                      <option value="match_host">匹配宿主（独占全部逻辑 CPU）</option>
                    </select>
                    {errors.cpu && <span className="field-error">{errors.cpu}</span>}
                  </span>
                </label>
              ) : (
                <NumberField label="处理器" value={draft.cpu_count} min={1} max={256} suffix="核" error={errors.cpu} onChange={value => mutate(next => { next.cpu_count = value; })} />
              )}
              <NumberField label="内存" value={draft.memory_mib} min={isMicrodroid ? 256 : 2048} max={1048576} suffix="MiB" error={errors.memory} onChange={value => mutate(next => { next.memory_mib = value; })} />
              {isMicrodroid && <p className="section-description">“匹配宿主”对应 AOSP AVF `match_host`，会占用全部逻辑 CPU 的独占租约；启动前必须先停止其他 Android 和 Microdroid 实例。</p>}
              <ResourceAdmissionCard capability={resourceCapability} loading={resourceCapabilityLoading} />
            </>
          )}
          {!isMicrodroid && section === 'display' && (
            <>
              <h2>Guest 显示</h2>
              <p className="section-description">这里设置 Android 的原生渲染分辨率；窗口大小由 Player 缩放控制。</p>
              <label className="setting-row">
                <span>常用预设</span>
                <select defaultValue="" onChange={event => { applyPreset(Number(event.target.value)); event.target.value = ''; }}>
                  <option value="" disabled>选择预设…</option>
                  {displayPresets.map((preset, index) => <option key={preset.label} value={index}>{preset.label}</option>)}
                </select>
              </label>
              <label className="setting-row">
                <span>方向</span>
                <select value={draft.display.orientation} onChange={event => mutate(next => { next.display.orientation = event.target.value as InstanceSpec['display']['orientation']; })}>
                  <option value="portrait">竖屏</option>
                  <option value="landscape">横屏</option>
                  <option value="reverse_portrait">反向竖屏</option>
                  <option value="reverse_landscape">反向横屏</option>
                </select>
              </label>
              <NumberField label="原生宽度" value={draft.display.width} min={320} max={8192} suffix="px" error={errors.width} onChange={value => mutate(next => { next.display.width = value; })} />
              <NumberField label="原生高度" value={draft.display.height} min={320} max={8192} suffix="px" error={errors.height} onChange={value => mutate(next => { next.display.height = value; })} />
              <NumberField label="DPI" value={draft.display.dpi} min={72} max={960} error={errors.dpi} onChange={value => mutate(next => { next.display.dpi = value; })} />
              <label className="setting-row">
                <span>刷新率</span>
                <select value={draft.display.refresh_rate_hz} onChange={event => mutate(next => { next.display.refresh_rate_hz = Number(event.target.value); })}>
                  {[30, 60, 90, 120].map(value => <option key={value} value={value}>{value} Hz</option>)}
                </select>
              </label>
              <div className="resolution-preview">
                {draft.display.width} × {draft.display.height} · {draft.display.dpi} DPI · {draft.display.refresh_rate_hz} Hz
              </div>
              <div className="secondary-display-heading">
                <span>
                  <strong>副显示器</strong>
                  <small>每个实例独立配置，最多 3 个；运行中修改会受控重启该实例，使 Android HWC 从冷启动配置完整建立显示器。</small>
                </span>
                <button
                  type="button"
                  className="secondary-button"
                  disabled={draft.display.secondary_displays.length >= 3}
                  onClick={() => mutate(next => {
                    const index = next.display.secondary_displays.length + 2;
                    next.display.secondary_displays.push({
                      id: crypto.randomUUID(),
                      name: `显示器 ${index}`,
                      width: 1920,
                      height: 1080,
                      dpi: 240,
                      refresh_rate_hz: 60,
                    });
                  })}
                >
                  <Plus size={15} />添加
                </button>
              </div>
              {errors.secondaryDisplays && <div className="inline-callout error">{errors.secondaryDisplays}</div>}
              <div className="secondary-display-list">
                {draft.display.secondary_displays.map((display, index) => (
                  <article className="secondary-display-card" key={display.id}>
                    <header>
                      <input
                        aria-label={`副显示器 ${index + 1} 名称`}
                        value={display.name}
                        onChange={event => mutate(next => { next.display.secondary_displays[index].name = event.target.value; })}
                      />
                      <button
                        type="button"
                        className="secondary-button danger"
                        aria-label={`移除 ${display.name}`}
                        onClick={() => mutate(next => { next.display.secondary_displays.splice(index, 1); })}
                      >移除</button>
                    </header>
                    <div className="secondary-display-fields">
                      <label>宽度<input type="number" min={320} max={8192} value={display.width} onChange={event => mutate(next => { next.display.secondary_displays[index].width = Number(event.target.value); })} /></label>
                      <label>高度<input type="number" min={320} max={8192} value={display.height} onChange={event => mutate(next => { next.display.secondary_displays[index].height = Number(event.target.value); })} /></label>
                      <label>DPI<input type="number" min={72} max={960} value={display.dpi} onChange={event => mutate(next => { next.display.secondary_displays[index].dpi = Number(event.target.value); })} /></label>
                      <label>刷新率<select value={display.refresh_rate_hz} onChange={event => mutate(next => { next.display.secondary_displays[index].refresh_rate_hz = Number(event.target.value) as 30 | 60 | 90 | 120; })}>{[30, 60, 90, 120].map(value => <option key={value} value={value}>{value} Hz</option>)}</select></label>
                    </div>
                  </article>
                ))}
              </div>
            </>
          )}
          {section === 'adb' && (
            <>
              <h2>{isMicrodroid ? '调试与 ADB' : 'Android / ADB'}</h2>
              <label className="setting-row">
                <span>ADB</span>
                <select value={draft.adb.mode} onChange={event => mutate(next => {
                  next.adb.mode = event.target.value as 'disabled' | 'loopback';
                  if (next.adb.mode === 'disabled') next.adb.host_port = null;
                })}>
                  <option value="loopback">本机回环</option>
                  <option value="disabled">禁用</option>
                </select>
              </label>
              {errors.adb && <div className="inline-callout warning">{errors.adb}</div>}
              <label className="setting-row">
                <span>ADB 可执行文件</span>
                <input className="mono-input" value={draft.adb.executable ?? ''} placeholder="自动发现" onChange={event => mutate(next => { next.adb.executable = event.target.value || null; })} />
              </label>
              {!isMicrodroid && <label className="setting-row">
                <span>APK 文件</span>
                <span className="apk-picker">
                  <input className="mono-input" value={apkPath} placeholder="尚未选择 APK" onChange={event => setApkPath(event.target.value)} />
                  <button type="button" className="secondary-button" disabled={busy} onClick={onChooseApk}>选择…</button>
                </span>
              </label>}
              {!isMicrodroid && <div className="setting-action-row">
                <button type="button" className="primary-button" disabled={busy || !androidActionsReady(record) || !apkPath} onClick={onInstallApk}>
                  <PackagePlus size={16} />安装 APK
                </button>
              </div>}
              <p className="section-description">{isMicrodroid
                ? 'Full 调试模式按 AOSP 契约挂载 adbd APEX，并为 EmptyPayload 与上传 Payload 提供实例独立的回环 ADB；None 模式必须禁用 ADB。'
                : '可使用系统文件选择器，也可以将 APK 文件直接拖入 HD 窗口。'}</p>
            </>
          )}
          {isMicrodroid && section === 'payload' && draft.microdroid && (
            <>
              <h2>工作负载</h2>
              <p className="section-description">每个 Microdroid 实例持有自己的 Payload、调试级别和加密存储配置。</p>
              <div className="setting-row">
                <span>Payload</span>
                <strong>{draft.microdroid.payload.kind === 'uploaded' ? '已上传 APK' : '内置 EmptyPayload'}</strong>
              </div>
              {draft.microdroid.payload.kind === 'uploaded' && (
                <div className="inline-callout">
                  SHA-256：<code>{draft.microdroid.payload.sha256}</code>
                </div>
              )}
              <div className="setting-action-row">
                <button type="button" className="primary-button" disabled={busy || active || dirty} onClick={onChoosePayload}>
                  <PackagePlus size={16} />选择 Payload APK…
                </button>
                {draft.microdroid.payload.kind === 'uploaded' && (
                  <button type="button" className="secondary-button" disabled={busy} onClick={() => mutate(next => {
                    if (next.microdroid) {
                      next.microdroid.payload = { kind: 'empty' };
                      next.microdroid.payload_extra_apk_count = null;
                      next.microdroid.extra_apks = [];
                    }
                  })}>
                    恢复 EmptyPayload
                  </button>
                )}
              </div>
              <p className="section-description">导入前必须停止实例且没有未保存设置。HD 会要求 APK 包含 assets/vm_config.json，校验 SHA-256，并在启动时自动生成 idsig。</p>
              {draft.microdroid.payload.kind === 'uploaded' && (
                <>
                  <div className="setting-row">
                    <span>额外 APK</span>
                    <strong>{draft.microdroid.extra_apks.length} / {draft.microdroid.payload_extra_apk_count ?? '待重新校验'}（最多 8）</strong>
                  </div>
                  {draft.microdroid.extra_apks.map((extra, index) => (
                    <div className="setting-row" key={extra.upload_id}>
                      <span>#{index + 1} · <code>{extra.sha256.slice(0, 16)}…</code></span>
                      <button type="button" className="secondary-button" disabled={busy} onClick={() => mutate(next => {
                        next.microdroid?.extra_apks.splice(index, 1);
                      })}>移除</button>
                    </div>
                  ))}
                  <div className="setting-action-row">
                    <button
                      type="button"
                      className="secondary-button"
                      disabled={busy || active || dirty || draft.microdroid.extra_apks.length >= 8
                        || (draft.microdroid.payload_extra_apk_count !== null
                          && draft.microdroid.extra_apks.length >= draft.microdroid.payload_extra_apk_count)}
                      onClick={() => post({ command: 'choose_microdroid_extra_apk' })}
                    >
                      <PackagePlus size={16} />添加额外 APK…
                    </button>
                  </div>
                  {errors.extraApks && <p className="field-error">{errors.extraApks}</p>}
                  <p className="section-description">HD 已有界读取主 Payload 的 assets/vm_config.json 并固定声明数量；选择顺序必须与 extra_apks 完全一致。运行时只传递受管上传文件描述符，不开放配置中的任意宿主路径，并会重新检查数量、摘要、签名及 idsig。</p>
                </>
              )}
              <label className="setting-row">
                <span>调试级别</span>
                <select value={draft.microdroid.debug_level} onChange={event => mutate(next => {
                  if (next.microdroid) {
                    next.microdroid.debug_level = event.target.value as 'none' | 'full';
                    if (next.microdroid.debug_level === 'none') {
                      next.adb.mode = 'disabled';
                      next.adb.host_port = null;
                    }
                  }
                })}>
                  <option value="full">Full（ADB / 控制台）</option>
                  <option value="none">None（发布模式）</option>
                </select>
              </label>
              <NumberField
                label="加密存储"
                value={draft.microdroid.encrypted_storage_mib ?? Number.NaN}
                min={10}
                max={4096}
                suffix="MiB"
                error={errors.storage}
                onChange={value => mutate(next => { if (next.microdroid) next.microdroid.encrypted_storage_mib = Number.isFinite(value) ? value : null; })}
              />
              <p className="section-description">容量仅在首次创建存储镜像时生效；已有镜像不能原地缩放，可停用后按原容量重新挂载。</p>
            </>
          )}
          {!isMicrodroid && section === 'artifacts' && (
            <>
              <h2>制品与启动</h2>
              {!draft.artifacts ? (
                <div className="empty-setting">
                  <p>使用启动器或 Host 提供的当前平台默认制品。推荐发布环境保持此选项。</p>
                  <button type="button" className="secondary-button" onClick={() => mutate(next => {
                    next.artifacts = { store_root: '', guest_bundle_digest: '', host_bundle_digest: '' };
                  })}>配置自定义制品</button>
                </div>
              ) : (
                <>
                  <label className={`setting-row ${errors.artifactRoot ? 'invalid' : ''}`}>
                    <span>制品仓库</span>
                    <span className="field-column">
                      <input className="mono-input" value={draft.artifacts.store_root} aria-invalid={Boolean(errors.artifactRoot)} onChange={event => mutate(next => { if (next.artifacts) next.artifacts.store_root = event.target.value; })} />
                      {errors.artifactRoot && <span className="field-error">{errors.artifactRoot}</span>}
                    </span>
                  </label>
                  <label className={`setting-row ${errors.guestDigest ? 'invalid' : ''}`}>
                    <span>Guest digest</span>
                    <span className="field-column">
                      <input className="mono-input" value={draft.artifacts.guest_bundle_digest} aria-invalid={Boolean(errors.guestDigest)} onChange={event => mutate(next => { if (next.artifacts) next.artifacts.guest_bundle_digest = event.target.value.trim(); })} />
                      {errors.guestDigest && <span className="field-error">{errors.guestDigest}</span>}
                    </span>
                  </label>
                  <label className={`setting-row ${errors.hostDigest ? 'invalid' : ''}`}>
                    <span>Host digest</span>
                    <span className="field-column">
                      <input className="mono-input" value={draft.artifacts.host_bundle_digest} aria-invalid={Boolean(errors.hostDigest)} onChange={event => mutate(next => { if (next.artifacts) next.artifacts.host_bundle_digest = event.target.value.trim(); })} />
                      {errors.hostDigest && <span className="field-error">{errors.hostDigest}</span>}
                    </span>
                  </label>
                  <button type="button" className="secondary-button danger" onClick={() => mutate(next => { next.artifacts = null; })}>恢复平台默认制品</button>
                </>
              )}
              {!isMicrodroid && <NumberField label="内核日志级别" value={draft.boot.kernel_log_level} min={0} max={7} error={errors.logLevel} onChange={value => mutate(next => { next.boot.kernel_log_level = value; })} />}
              {!isMicrodroid && <label className="setting-row">
                <span>开机动画</span>
                <input type="checkbox" checked={draft.boot.boot_animation} onChange={event => mutate(next => { next.boot.boot_animation = event.target.checked; })} />
              </label>}
            </>
          )}
          {!isMicrodroid && section === 'devices' && (
            <>
              <h2>设备模拟</h2>
              {Object.entries(draft.devices).filter(([key]) => key !== 'touchpad').map(([key, enabled]) => {
                const capability = capabilityMap.get(key);
                const unsupported = !capabilitiesLoading && (!capability || !capability.available);
                const disabled = capabilitiesLoading || unsupported;
                return (
                  <label className={`setting-row ${unsupported ? 'unsupported-setting' : ''}`} key={key}>
                    <span className="setting-label-stack">
                      <span>{deviceLabels[key] ?? key}</span>
                      <small>
                        {capabilitiesLoading
                          ? '正在检测当前主机能力…'
                          : capability
                            ? `${deviceBackendLabels[capability.backend]} · ${capability.available ? capability.features.join(' / ') : '当前主机不可用'}`
                            : '当前主机未报告此设备能力'}
                      </small>
                    </span>
                    <input
                      type="checkbox"
                      checked={enabled}
                      disabled={disabled}
                      aria-describedby={disabled ? `unsupported-${key}` : undefined}
                      onChange={event => mutate(next => {
                        next.devices[key] = event.target.checked;
                        if (key === 'audio' && !event.target.checked) next.host_audio_input = 'disabled';
                      })}
                    />
                    {disabled && <span className="sr-only" id={`unsupported-${key}`}>{capabilitiesLoading ? '正在检测当前主机设备能力' : '当前主机不支持此设备模拟'}</span>}
                  </label>
                );
              })}
              <h3>宿主设备透传</h3>
              <p className="section-description">虚拟音频设备与宿主麦克风权限分开管理。宿主采集默认关闭，并且只随当前实例启动。</p>
              <label className={`setting-row ${!capabilityMap.get('audio')?.features.includes('host_default_microphone') ? 'unsupported-setting' : ''}`}>
                <span className="setting-label-stack">
                  <span>麦克风输入</span>
                  <small>{capabilityMap.get('audio')?.features.includes('host_default_microphone')
                    ? '系统默认麦克风 · Host PCM → virtio-snd'
                    : '当前平台尚无已验证的宿主麦克风数据面'}</small>
                </span>
                <select
                  value={draft.host_audio_input}
                  disabled={capabilitiesLoading || !draft.devices.audio}
                  onChange={event => mutate(next => { next.host_audio_input = event.target.value as InstanceSpec['host_audio_input']; })}
                >
                  <option value="disabled">关闭（默认）</option>
                  <option value="default_microphone" disabled={!capabilityMap.get('audio')?.features.includes('host_default_microphone')}>使用系统默认麦克风</option>
                </select>
              </label>
            </>
          )}
          {section === 'advanced' && (
            <>
              <h2>高级</h2>
              {!isMicrodroid && <NumberField label="Kernel log level" value={draft.boot.kernel_log_level} min={0} max={7} error={errors.logLevel} onChange={value => mutate(next => { next.boot.kernel_log_level = value; })} />}
              {!isMicrodroid && <NumberField label="Panic timeout" value={draft.boot.panic_timeout_seconds} min={0} max={300} suffix="秒" error={errors.panic} onChange={value => mutate(next => { next.boot.panic_timeout_seconds = value; })} />}
              {!isMicrodroid && <label className="setting-row">
                <span>显示 Host FPS</span>
                <input type="checkbox" checked={draft.display.show_host_fps} onChange={event => mutate(next => { next.display.show_host_fps = event.target.checked; })} />
              </label>}
              {isMicrodroid && <p className="section-description">Microdroid 是无图形工作负载，不提供显示、旋转、FPS 或设备模拟设置。</p>}
            </>
          )}
        </div>
      </div>
      <ConfirmDialog
        open={confirmRestart}
        title={`保存设置并重启 ${isMicrodroid ? 'Microdroid' : 'Android'}？`}
        description="HD 将安全停止当前实例、保存全部更改，然后重新启动。未保存的 Guest 数据可能丢失。"
        confirmLabel="保存并重启"
        busy={busy}
        onCancel={() => setConfirmRestart(false)}
        onConfirm={() => {
          setConfirmRestart(false);
          onSave(draft, true, record.status.revision);
        }}
      />
    </section>
  );
}

function CapabilityState({
  enabled,
  adbReady,
  capability,
  loading,
}: {
  enabled: boolean;
  adbReady: boolean;
  capability?: DeviceCapability;
  loading: boolean;
}) {
  const available = Boolean(capability?.available);
  const runtime = capability?.runtime;
  const state = loading
    ? '检测中'
    : !capability
      ? '能力未知'
      : !available
        ? '当前主机未配置'
        : !enabled
          ? '实例未启用'
          : !adbReady
            ? '已配置 · 等待启动'
            : !runtime?.probed
              ? '运行态未验证'
            : runtime?.verified
              ? runtime.controllable
                ? '已验证 · 可控制'
                : runtime.running
                  ? '已验证 · 运行中'
                  : '已验证 · 离线基线'
              : runtime?.running
                ? '运行中 · 未验证'
                : runtime?.configured
                  ? '已配置 · 未运行'
                  : '未配置';
  return (
    <span className={`capability-state ${runtime?.verified && enabled ? 'available' : ''}`}>
      {state}
    </span>
  );
}

function CapabilitySummary({ capability, loading }: { capability?: DeviceCapability; loading: boolean }) {
  if (loading) return <p className="device-capability-summary">正在检测当前主机设备能力…</p>;
  if (!capability) return <p className="device-capability-summary">当前主机未返回该设备的能力信息。</p>;
  return (
    <p className="device-capability-summary" title={`${capability.boundary}\n${capability.runtime.detail}`}>
      <strong>{deviceBackendLabels[capability.backend]}</strong>
      <span>{capability.features.length ? capability.features.join(' · ') : '无可声明功能'}</span>
      <small>{capability.boundary}</small>
      <small>{capability.runtime.detail}</small>
    </p>
  );
}

function DevicesPage({
  record,
  busy,
  locationRoutePath,
  setLocationRoutePath,
  deviceCapabilities,
  capabilitiesLoading,
  networkSetup,
}: {
  record: InstanceRecord | null;
  busy: boolean;
  locationRoutePath: string;
  setLocationRoutePath: (path: string) => void;
  deviceCapabilities: DeviceCapability[];
  capabilitiesLoading: boolean;
  networkSetup: NetworkSetupState;
}) {
  const [latitude, setLatitude] = useState('37.4219999');
  const [longitude, setLongitude] = useState('-122.0840577');
  const [altitudeMeters, setAltitudeMeters] = useState(5);
  const [accuracyMeters, setAccuracyMeters] = useState(5);
  const [routeIntervalMs, setRouteIntervalMs] = useState(1000);
  const [routeRepeat, setRouteRepeat] = useState(false);
  const [battery, setBattery] = useState(100);
  const [charging, setCharging] = useState(true);
  const [temperatureCelsius, setTemperatureCelsius] = useState(25);
  const [latency, setLatency] = useState(0);
  const [lossPercent, setLossPercent] = useState(0);
  const [bandwidthKbps, setBandwidthKbps] = useState('');
  const [sensor, setSensor] = useState('accelerometer');
  const [sensorValues, setSensorValues] = useState('0,0,9806650');
  const [sensorDurationMs, setSensorDurationMs] = useState(0);
  const [poseXDegrees, setPoseXDegrees] = useState((record?.sensor_pose?.x_millidegrees ?? 0) / 1000);
  const [poseYDegrees, setPoseYDegrees] = useState((record?.sensor_pose?.y_millidegrees ?? 0) / 1000);
  const [poseZDegrees, setPoseZDegrees] = useState((record?.sensor_pose?.z_millidegrees ?? 0) / 1000);
  const [poseTransitionMs, setPoseTransitionMs] = useState(record?.sensor_pose?.transition_ms ?? 200);
  const [peerName, setPeerName] = useState('HD GATT peer');
  const [peerKind, setPeerKind] = useState<'gatt' | 'beacon' | 'scripted_beacon' | 'hid_keyboard'>('gatt');
  const [beaconAdvertisingData, setBeaconAdvertisingData] = useState('02010605FF4C000215');
  const [scriptedBeaconTimeline, setScriptedBeaconTimeline] = useState('1000:02010605FF4C000215\n1000:02010605FF4C000216');
  const [scriptedBeaconRepeat, setScriptedBeaconRepeat] = useState(true);
  const [hidKeyboardUsage, setHidKeyboardUsage] = useState(4);
  const [hidKeyboardModifiers, setHidKeyboardModifiers] = useState(0);
  const [bluetoothCaptureDurationMs, setBluetoothCaptureDurationMs] = useState(5000);
  const [peerId, setPeerId] = useState(() => crypto.randomUUID());
  const [confirmNetworkSetup, setConfirmNetworkSetup] = useState(false);
  const [ndef, setNdef] = useState('D1010B5402656E48656C6C6F204844');
  const [uwbDistanceCm, setUwbDistanceCm] = useState(record?.uwb_ranging?.distance_cm ?? 250);
  const [modemOperatorNumeric, setModemOperatorNumeric] = useState(record?.modem_state?.operator_numeric ?? '00101');
  const [modemOperatorLongName, setModemOperatorLongName] = useState(record?.modem_state?.operator_long_name ?? 'HD Mobile');
  const [modemOperatorShortName, setModemOperatorShortName] = useState(record?.modem_state?.operator_short_name ?? 'HD');
  const [modemSignalStrength, setModemSignalStrength] = useState(record?.modem_state?.signal_strength ?? 20);
  const [modemRegistered, setModemRegistered] = useState(record?.modem_state?.registered ?? true);
  const [formError, setFormError] = useState('');
  const capabilityMap = useMemo(
    () => new Map(deviceCapabilities.map(capability => [capability.id, capability])),
    [deviceCapabilities],
  );
  const sensorOptions = useMemo(
    () => ['accelerometer', 'gyroscope', 'magnetometer', 'light', 'proximity']
      .filter(value => capabilityMap.get('sensors')?.features.includes(value)),
    [capabilityMap],
  );

  useEffect(() => {
    if (sensorOptions.length && !sensorOptions.includes(sensor)) {
      setSensor(sensorOptions[0]);
      setSensorValues('0,0,9806650');
    }
  }, [sensor, sensorOptions]);

  useEffect(() => {
    setLatitude('37.4219999');
    setLongitude('-122.0840577');
    setAltitudeMeters(5);
    setAccuracyMeters(5);
    setRouteIntervalMs(1000);
    setRouteRepeat(false);
    setBattery(100);
    setCharging(true);
    setTemperatureCelsius(25);
    setLatency(0);
    setLossPercent(0);
    setBandwidthKbps('');
    setSensor('accelerometer');
    setSensorValues('0,0,9806650');
    setSensorDurationMs(0);
    setPeerName('HD GATT peer');
    setPeerKind('gatt');
    setBeaconAdvertisingData('02010605FF4C000215');
    setScriptedBeaconTimeline('1000:02010605FF4C000215\n1000:02010605FF4C000216');
    setScriptedBeaconRepeat(true);
    setBluetoothCaptureDurationMs(5000);
    setPeerId(crypto.randomUUID());
    setNdef('D1010B5402656E48656C6C6F204844');
    setFormError('');
    setConfirmNetworkSetup(false);
  }, [record?.spec.id]);

  useEffect(() => {
    setPoseXDegrees((record?.sensor_pose?.x_millidegrees ?? 0) / 1000);
    setPoseYDegrees((record?.sensor_pose?.y_millidegrees ?? 0) / 1000);
    setPoseZDegrees((record?.sensor_pose?.z_millidegrees ?? 0) / 1000);
    setPoseTransitionMs(record?.sensor_pose?.transition_ms ?? 200);
  }, [
    record?.spec.id,
    record?.sensor_pose?.x_millidegrees,
    record?.sensor_pose?.y_millidegrees,
    record?.sensor_pose?.z_millidegrees,
    record?.sensor_pose?.transition_ms,
  ]);

  useEffect(() => {
    setUwbDistanceCm(record?.uwb_ranging?.distance_cm ?? 250);
  }, [record?.spec.id, record?.uwb_ranging?.distance_cm]);

  useEffect(() => {
    setModemOperatorNumeric(record?.modem_state?.operator_numeric ?? '00101');
    setModemOperatorLongName(record?.modem_state?.operator_long_name ?? 'HD Mobile');
    setModemOperatorShortName(record?.modem_state?.operator_short_name ?? 'HD');
    setModemSignalStrength(record?.modem_state?.signal_strength ?? 20);
    setModemRegistered(record?.modem_state?.registered ?? true);
  }, [
    record?.spec.id,
    record?.modem_state?.operator_numeric,
    record?.modem_state?.operator_long_name,
    record?.modem_state?.operator_short_name,
    record?.modem_state?.signal_strength,
    record?.modem_state?.registered,
  ]);

  if (!record) return <section className="empty-page"><h1>设备模拟</h1><p>请先选择一个实例。</p></section>;

  const adbReady = androidActionsReady(record);
  const enabled = (key: string) => Boolean(record.spec.devices[key]);
  const hostCapability = (key: string) => capabilityMap.get(key);
  const canUse = (key: string) => {
    const capability = hostCapability(key);
    return !busy
      && adbReady
      && enabled(key)
      && Boolean(capability?.available)
      && Boolean(capability?.runtime.controllable)
      && Boolean(capability?.features.includes('runtime_control'));
  };
  const canUseSensorPose = canUse('sensors')
    && Boolean(hostCapability('sensors')?.features.includes('three_axis_pose'));
  const canInjectIndividualSensor = canUse('sensors') && sensorOptions.length > 0;
  const send = (action: Record<string, unknown>) => {
    setFormError('');
    post({ command: 'action', action });
  };
  const applyLocation = () => {
    const lat = Number(latitude);
    const lon = Number(longitude);
    if (!Number.isFinite(lat) || lat < -90 || lat > 90
      || !Number.isFinite(lon) || lon < -180 || lon > 180
      || !Number.isFinite(altitudeMeters) || altitudeMeters < -1000 || altitudeMeters > 100000
      || !Number.isFinite(accuracyMeters) || accuracyMeters <= 0 || accuracyMeters > 1000) {
      setFormError('纬度应为 -90–90、经度为 -180–180；高度为 -1000–100000 m，精度应为 0–1000 m。');
      return;
    }
    send({ action: 'set_location', parameters: { location: {
      latitude_e7: Math.round(lat * 10_000_000),
      longitude_e7: Math.round(lon * 10_000_000),
      altitude_mm: Math.round(altitudeMeters * 1000),
      accuracy_mm: Math.round(accuracyMeters * 1000),
    } } });
  };
  const applyBattery = () => {
    if (!Number.isInteger(battery) || battery < 0 || battery > 100
      || !Number.isFinite(temperatureCelsius) || temperatureCelsius < -50 || temperatureCelsius > 100) {
      setFormError('电量应为 0–100 的整数，温度应为 -50–100 °C。');
      return;
    }
    send({ action: 'set_battery', parameters: { battery: {
      level_percent: battery,
      charging,
      temperature_deci_celsius: Math.round(temperatureCelsius * 10),
    } } });
  };
  const applyNetwork = () => {
    const bandwidth = bandwidthKbps.trim() === '' ? null : Number(bandwidthKbps);
    if (!Number.isInteger(latency) || latency < 0 || latency > 60000
      || !Number.isFinite(lossPercent) || lossPercent < 0 || lossPercent > 100
      || (bandwidth !== null && (!Number.isInteger(bandwidth) || bandwidth <= 0 || bandwidth > 4294967295))) {
      setFormError('延迟应为 0–60000 ms，丢包率应为 0–100%，带宽留空表示不限速，否则必须为正整数。');
      return;
    }
    send({ action: 'set_network_condition', parameters: { condition: {
      latency_ms: latency,
      loss_basis_points: Math.round(lossPercent * 100),
      bandwidth_kbps: bandwidth,
    } } });
  };
  const applyUwbRanging = () => {
    if (!Number.isInteger(uwbDistanceCm) || uwbDistanceCm < 1 || uwbDistanceCm > 65535) {
      setFormError('UWB 距离应为 1–65535 厘米的整数。');
      return;
    }
    send({
      action: 'set_uwb_ranging',
      parameters: { ranging: { distance_cm: uwbDistanceCm } },
    });
  };
  const applyModemState = () => {
    const numeric = modemOperatorNumeric.trim();
    const longName = modemOperatorLongName.trim();
    const shortName = modemOperatorShortName.trim();
    const unsafeName = (name: string) => /["\r\n]/.test(name);
    if (!/^\d{5,6}$/.test(numeric)
      || !longName || longName.length > 32 || unsafeName(longName)
      || !shortName || shortName.length > 16 || unsafeName(shortName)
      || !Number.isInteger(modemSignalStrength) || modemSignalStrength < 0 || modemSignalStrength > 31) {
      setFormError('运营商代码应为 5–6 位数字；名称不能为空且不能含引号/换行；信号强度应为 0–31 的整数。');
      return;
    }
    send({
      action: 'set_modem_state',
      parameters: { modem: {
        operator_numeric: numeric,
        operator_long_name: longName,
        operator_short_name: shortName,
        signal_strength: modemSignalStrength,
        registered: modemRegistered,
      } },
    });
  };
  const injectSensor = () => {
    const values = sensorValues.split(',').map(value => Number(value.trim()));
    const expectedValues = ['light', 'proximity'].includes(sensor) ? 1 : 3;
    const validDuration = sensorDurationMs === 0
      || (sensorDurationMs >= 200 && sensorDurationMs <= 600000);
    if (values.length !== expectedValues || values.some(value => !Number.isFinite(value))
      || !Number.isInteger(sensorDurationMs) || !validDuration) {
      setFormError(`${sensor} 需要 ${expectedValues} 个用逗号分隔的数值；持续时间为 0（持续覆盖）或 200–600000 ms。`);
      return;
    }
    send({ action: 'inject_sensor', parameters: { injection: {
      sensor,
      values_microunits: values,
      duration_ms: sensorDurationMs,
    } } });
  };
  const applySensorPose = () => {
    const angles = [poseXDegrees, poseYDegrees, poseZDegrees];
    if (angles.some(angle => !Number.isFinite(angle) || angle < -180 || angle > 180)
      || !Number.isInteger(poseTransitionMs) || poseTransitionMs < 200 || poseTransitionMs > 10000) {
      setFormError('三轴姿态角应为 -180°–180°，过渡时间应为 200–10000 ms 的整数。');
      return;
    }
    send({ action: 'set_sensor_pose', parameters: { pose: {
      x_millidegrees: Math.round(poseXDegrees * 1000),
      y_millidegrees: Math.round(poseYDegrees * 1000),
      z_millidegrees: Math.round(poseZDegrees * 1000),
      transition_ms: poseTransitionMs,
    } } });
  };
  const presentNfc = (command: 'present_type2' | 'present_type4') => {
    const value = ndef.trim();
    if (!value || value.length % 2 !== 0 || !/^[0-9a-f]+$/i.test(value)) {
      setFormError('NDEF 必须是长度为偶数的十六进制字符串。');
      return;
    }
    send({ action: 'nfc_tag', parameters: { action: { command, ndef_hex: value } } });
  };
  const createBluetoothPeer = () => {
    const name = peerName.trim();
    if (!name || new TextEncoder().encode(name).length > 128) {
      setFormError('Bluetooth Peer 名称应为 1–128 字节。');
      return;
    }
    const validateAdvertisingData = (input: string) => {
      const value = input.trim();
      if (!value || value.length > 62 || value.length % 2 !== 0 || !/^[0-9a-f]+$/i.test(value)) return null;
      const bytes = value.match(/.{2}/g)?.map(byte => Number.parseInt(byte, 16)) ?? [];
      let offset = 0;
      while (offset < bytes.length && bytes[offset] > 0 && offset + bytes[offset] + 1 <= bytes.length) {
        offset += bytes[offset] + 1;
      }
      return offset === bytes.length ? value.toUpperCase() : null;
    };
    if (peerKind === 'hid_keyboard') {
      send({ action: 'bluetooth_peer', parameters: { action: {
        command: 'create_hid_keyboard', peer_id: peerId, name,
      } } });
    } else if (peerKind === 'beacon') {
      const value = validateAdvertisingData(beaconAdvertisingData);
      if (!value) {
        setFormError('Beacon 广播数据必须是 1–31 字节的十六进制 BLE AD 结构。');
        return;
      }
      send({ action: 'bluetooth_peer', parameters: { action: {
        command: 'create_beacon', peer_id: peerId, name, advertising_data_hex: value,
      } } });
    } else if (peerKind === 'scripted_beacon') {
      const lines = scriptedBeaconTimeline.split(/\r?\n/).map(line => line.trim()).filter(Boolean);
      const frames = lines.map(line => {
        const match = /^(\d+)\s*:\s*(.+)$/.exec(line);
        if (!match) return null;
        const durationMs = Number(match[1]);
        const advertisingDataHex = validateAdvertisingData(match[2]);
        if (!Number.isInteger(durationMs) || durationMs < 20 || durationMs > 60000 || !advertisingDataHex) return null;
        return { advertising_data_hex: advertisingDataHex, duration_ms: durationMs };
      });
      const totalDurationMs = frames.reduce((total, frame) => total + (frame?.duration_ms ?? 0), 0);
      if (!lines.length || lines.length > 64 || frames.some(frame => frame === null) || totalDurationMs > 600000) {
        setFormError('广播序列应为 1–64 行“持续毫秒:BLE AD hex”，单帧 20–60000 ms、总计不超过 10 分钟。');
        return;
      }
      send({ action: 'bluetooth_peer', parameters: { action: {
        command: 'create_scripted_beacon', peer_id: peerId, name, frames, repeat: scriptedBeaconRepeat,
      } } });
    } else {
      send({ action: 'bluetooth_peer', parameters: { action: {
        command: 'create_gatt_peer', peer_id: peerId, name,
      } } });
    }
    setPeerId(crypto.randomUUID());
  };
  const captureBluetoothHci = () => {
    if (!Number.isInteger(bluetoothCaptureDurationMs)
      || bluetoothCaptureDurationMs < 1000
      || bluetoothCaptureDurationMs > 30000) {
      setFormError('HCI 抓包时长应为 1000–30000 ms 的整数。');
      return;
    }
    send({ action: 'bluetooth_peer', parameters: { action: {
      command: 'capture_hci',
      capture_id: crypto.randomUUID(),
      duration_ms: bluetoothCaptureDurationMs,
    } } });
  };

  return (
    <section className="simple-page">
      <div className="page-heading">
        <div><h1>设备模拟</h1><p>可用的运行时控制会立即作用于 Android；其余设备随实例配置启动。</p></div>
        <span className={`adb-badge ${record.adb_ready ? 'ready' : ''}`}>{record.adb_ready ? 'ADB 已就绪' : 'ADB 连接中'}</span>
      </div>
      {formError && <div className="inline-callout error" role="alert">{formError}</div>}
      <div className="device-grid">
        <article className="device-card">
          <h2><MapPin size={17} />定位 <CapabilityState enabled={enabled('gnss')} adbReady={record.adb_ready} capability={hostCapability('gnss')} loading={capabilitiesLoading} /></h2>
          <CapabilitySummary capability={hostCapability('gnss')} loading={capabilitiesLoading} />
          <label><span>纬度</span><input disabled={!canUse('gnss')} type="number" step="0.0000001" value={latitude} onChange={event => setLatitude(event.target.value)} /></label>
          <label><span>经度</span><input disabled={!canUse('gnss')} type="number" step="0.0000001" value={longitude} onChange={event => setLongitude(event.target.value)} /></label>
          <label><span>高度 m</span><input disabled={!canUse('gnss')} type="number" min={-1000} max={100000} step={0.1} value={altitudeMeters} onChange={event => setAltitudeMeters(Number(event.target.value))} /></label>
          <label><span>精度 m</span><input disabled={!canUse('gnss')} type="number" min={0.001} max={1000} step={0.1} value={accuracyMeters} onChange={event => setAccuracyMeters(Number(event.target.value))} /></label>
          <button type="button" disabled={!canUse('gnss')} onClick={applyLocation}>应用定位</button>
          <div className="device-subsection">
            <strong>路线回放</strong>
            <label><span>GPX / KML</span><input disabled={!canUse('gnss') || Boolean(record.location_route)} value={locationRoutePath} placeholder="选择或输入路线文件路径" onChange={event => setLocationRoutePath(event.target.value)} /></label>
            <button type="button" disabled={!canUse('gnss') || Boolean(record.location_route)} onClick={() => post({ command: 'choose_location_route' })}>选择路线…</button>
            <label><span>点间隔 ms</span><input disabled={!canUse('gnss') || Boolean(record.location_route)} type="number" min={250} max={60000} step={250} value={routeIntervalMs} onChange={event => setRouteIntervalMs(Number(event.target.value))} /></label>
            <label className="compact-check"><span>循环回放</span><input disabled={!canUse('gnss') || Boolean(record.location_route)} type="checkbox" checked={routeRepeat} onChange={event => setRouteRepeat(event.target.checked)} /></label>
            {record.location_route ? (
              <>
                <p className="route-status"><strong>{record.location_route.name}</strong><span>{record.location_route.current_point} / {record.location_route.point_count} · {record.location_route.state === 'paused' ? '已暂停' : '回放中'}</span></p>
                <div className="button-row">
                  <button type="button" disabled={busy} onClick={() => send({ action: record.location_route?.state === 'paused' ? 'resume_location_route' : 'pause_location_route' })}>{record.location_route.state === 'paused' ? '继续' : '暂停'}</button>
                  <button type="button" disabled={busy} onClick={() => send({ action: 'stop_location_route' })}>停止</button>
                </div>
              </>
            ) : (
              <>
                {record.last_location_route && (
                  <p className={`route-status ${record.last_location_route.reason === 'failed' ? 'failed' : ''}`}>
                    <strong>最近：{record.last_location_route.name}</strong>
                    <span>{record.last_location_route.reason === 'completed' ? '已完成' : record.last_location_route.reason === 'stopped' ? '已停止' : `失败 · ${record.last_location_route.error_code ?? '未知错误'}`}</span>
                  </p>
                )}
                <button
                  type="button"
                  disabled={!canUse('gnss') || !locationRoutePath.trim() || !Number.isInteger(routeIntervalMs) || routeIntervalMs < 250 || routeIntervalMs > 60000}
                  onClick={() => post({ command: 'start_location_route', interval_ms: routeIntervalMs, repeat: routeRepeat })}
                >开始回放</button>
              </>
            )}
            <small>最多 2,048 个点；文件上限 2 MiB。新路线或手动定位会停止当前回放。</small>
          </div>
        </article>
        <article className="device-card">
          <h2><Battery size={17} />电池 <CapabilityState enabled={enabled('power')} adbReady={record.adb_ready} capability={hostCapability('power')} loading={capabilitiesLoading} /></h2>
          <CapabilitySummary capability={hostCapability('power')} loading={capabilitiesLoading} />
          <label><span>电量</span><input disabled={!canUse('power')} type="number" min={0} max={100} value={battery} onChange={event => setBattery(Number(event.target.value))} /></label>
          <label className="compact-check"><span>正在充电</span><input disabled={!canUse('power')} type="checkbox" checked={charging} onChange={event => setCharging(event.target.checked)} /></label>
          <label><span>温度 °C</span><input disabled={!canUse('power')} type="number" min={-50} max={100} step={0.1} value={temperatureCelsius} onChange={event => setTemperatureCelsius(Number(event.target.value))} /></label>
          <button type="button" disabled={!canUse('power')} onClick={applyBattery}>应用电池</button>
        </article>
        <article className="device-card">
          <h2><Wifi size={17} />网络 <CapabilityState enabled={enabled('network')} adbReady={record.adb_ready} capability={hostCapability('network')} loading={capabilitiesLoading} /></h2>
          <CapabilitySummary capability={hostCapability('network')} loading={capabilitiesLoading} />
          {networkSetup.supported && (
            <div className={`inline-callout network-setup-callout ${networkSetup.network_usable ? '' : 'error'}`}>
              <strong>{
                networkSetup.health === 'ready' ? 'macOS 网络服务已就绪'
                  : networkSetup.health === 'maintenance' ? '网络可用，兼容服务需要维护'
                    : networkSetup.health === 'degraded' ? 'VPN 上行链路未就绪'
                      : networkSetup.health === 'offline' ? 'socket_vmnet 未运行'
                        : '正在检查 macOS 网络服务'
              }</strong>
              <span>{networkSetup.detail}</span>
              <small>
                出口：{networkSetup.egress} · socket_vmnet：{networkSetup.socket_vmnet ? '已连接' : '未连接'}
                {networkSetup.vpn_nat_required ? ` · VPN NAT：${networkSetup.nat === 'active' ? '已激活' : '未激活'}` : ''}
              </small>
              <div className="button-row">
                <button type="button" disabled={busy} onClick={() => post({ command: 'network_setup_refresh' })}>刷新状态</button>
                {networkSetup.service_action !== 'none' && networkSetup.service_action !== 'manual_repair' && (
                  <button type="button" disabled={busy} onClick={() => setConfirmNetworkSetup(true)}>
                    {networkSetup.service_action === 'install' ? '安装网络服务'
                      : networkSetup.service_action === 'upgrade' ? '升级网络服务'
                        : '修复网络服务'}
                  </button>
                )}
              </div>
            </div>
          )}
          <label><span>延迟 ms</span><input disabled={!canUse('network')} type="number" min={0} value={latency} onChange={event => setLatency(Number(event.target.value))} /></label>
          <label><span>丢包率 %</span><input disabled={!canUse('network')} type="number" min={0} max={100} step={0.1} value={lossPercent} onChange={event => setLossPercent(Number(event.target.value))} /></label>
          <label><span>带宽 Kbps</span><input disabled={!canUse('network')} type="number" min={1} placeholder="不限速" value={bandwidthKbps} onChange={event => setBandwidthKbps(event.target.value)} /></label>
          <button type="button" disabled={!canUse('network')} onClick={applyNetwork}>应用网络</button>
        </article>
        <article className="device-card">
          <h2><Activity size={17} />传感器 <CapabilityState enabled={enabled('sensors')} adbReady={record.adb_ready} capability={hostCapability('sensors')} loading={capabilitiesLoading} /></h2>
          <CapabilitySummary capability={hostCapability('sensors')} loading={capabilitiesLoading} />
          <div className="device-runtime-note">三轴姿态会按 AOSP `Rz × Ry × Rx` 模型原子生成加速度、磁场和角速度。</div>
          {(['X', 'Y', 'Z'] as const).map(axis => {
            const value = axis === 'X' ? poseXDegrees : axis === 'Y' ? poseYDegrees : poseZDegrees;
            const setter = axis === 'X' ? setPoseXDegrees : axis === 'Y' ? setPoseYDegrees : setPoseZDegrees;
            return <label key={axis}><span>{axis} 轴 {value.toFixed(1)}°</span><input disabled={!canUseSensorPose} type="range" min={-180} max={180} step={0.1} value={value} onChange={event => setter(Number(event.target.value))} /></label>;
          })}
          <label><span>姿态过渡 ms</span><input disabled={!canUseSensorPose} type="number" min={200} max={10000} value={poseTransitionMs} onChange={event => setPoseTransitionMs(Number(event.target.value))} /></label>
          <div className="button-row sensor-preset-row" aria-label="三轴姿态预设">
            <button type="button" disabled={!canUseSensorPose} onClick={() => { setPoseXDegrees(0); setPoseYDegrees(0); setPoseZDegrees(0); }}>直立</button>
            <button type="button" disabled={!canUseSensorPose} onClick={() => { setPoseXDegrees(-90); setPoseYDegrees(0); setPoseZDegrees(0); }}>左倾</button>
            <button type="button" disabled={!canUseSensorPose} onClick={() => { setPoseXDegrees(90); setPoseYDegrees(0); setPoseZDegrees(0); }}>右倾</button>
            <button type="button" disabled={!canUseSensorPose} onClick={() => { setPoseXDegrees(180); setPoseYDegrees(0); setPoseZDegrees(0); }}>倒置</button>
          </div>
          <button type="button" disabled={!canUseSensorPose} onClick={applySensorPose}>应用三轴姿态</button>
          {sensorOptions.length > 0 && <>
            <hr />
            <select disabled={!canInjectIndividualSensor} value={sensor} onChange={event => setSensor(event.target.value)}>
              {sensorOptions.map(value => <option key={value}>{value}</option>)}
            </select>
            <label><span>微单位值</span><input disabled={!canInjectIndividualSensor} value={sensorValues} onChange={event => setSensorValues(event.target.value)} /></label>
            <label><span>持续时间 ms（0 = 持续）</span><input disabled={!canInjectIndividualSensor} type="number" min={0} max={600000} value={sensorDurationMs} onChange={event => setSensorDurationMs(Number(event.target.value))} /></label>
            <div className="button-row sensor-preset-row" aria-label="固定姿态预设">
              <button type="button" disabled={!canInjectIndividualSensor} onClick={() => { setSensor('accelerometer'); setSensorValues('-9806650,0,0'); }}>左侧</button>
              <button type="button" disabled={!canInjectIndividualSensor} onClick={() => { setSensor('accelerometer'); setSensorValues('0,9806650,0'); }}>直立</button>
              <button type="button" disabled={!canInjectIndividualSensor} onClick={() => { setSensor('accelerometer'); setSensorValues('9806650,0,0'); }}>右侧</button>
              <button type="button" disabled={!canInjectIndividualSensor} onClick={() => { setSensor('accelerometer'); setSensorValues('0,-9806650,0'); }}>倒置</button>
            </div>
            <button type="button" disabled={!canInjectIndividualSensor} onClick={injectSensor}>注入传感器</button>
          </>}
        </article>
        <article className="device-card">
          <h2><Bluetooth size={17} />Bluetooth <CapabilityState enabled={enabled('bluetooth')} adbReady={record.adb_ready} capability={hostCapability('bluetooth')} loading={capabilitiesLoading} /></h2>
          <CapabilitySummary capability={hostCapability('bluetooth')} loading={capabilitiesLoading} />
          <label><span>Peer 类型</span><select disabled={!canUse('bluetooth')} value={peerKind} onChange={event => setPeerKind(event.target.value as 'gatt' | 'beacon' | 'scripted_beacon' | 'hid_keyboard')}><option value="gatt">GATT Peer</option><option value="hid_keyboard">HID 键盘</option><option value="beacon">BLE Beacon</option><option value="scripted_beacon">广播序列 Beacon</option></select></label>
          <label><span>Peer 名称</span><input disabled={!canUse('bluetooth')} value={peerName} onChange={event => setPeerName(event.target.value)} /></label>
          {peerKind === 'beacon' && <label><span>广播数据 hex</span><input disabled={!canUse('bluetooth')} className="mono-input" maxLength={62} value={beaconAdvertisingData} onChange={event => setBeaconAdvertisingData(event.target.value)} /></label>}
          {peerKind === 'scripted_beacon' && <><label><span>广播序列</span><textarea disabled={!canUse('bluetooth')} className="mono-input" rows={4} value={scriptedBeaconTimeline} onChange={event => setScriptedBeaconTimeline(event.target.value)} /></label><label className="check-line"><input disabled={!canUse('bluetooth')} type="checkbox" checked={scriptedBeaconRepeat} onChange={event => setScriptedBeaconRepeat(event.target.checked)} />循环播放</label></>}
          <button type="button" disabled={!canUse('bluetooth') || !peerName.trim()} onClick={createBluetoothPeer}>创建 {peerKind === 'gatt' ? 'GATT Peer' : peerKind === 'hid_keyboard' ? 'HID 键盘' : peerKind === 'beacon' ? 'Beacon' : '广播序列'}</button>
          {(record.bluetooth_peers ?? []).some(peer => peer.kind === 'hid_keyboard') && <div className="button-row">
            <label><span>HID usage</span><input disabled={!canUse('bluetooth')} type="number" min={4} max={231} value={hidKeyboardUsage} onChange={event => setHidKeyboardUsage(Number(event.target.value))} /></label>
            <label><span>修饰键位图</span><input disabled={!canUse('bluetooth')} type="number" min={0} max={255} value={hidKeyboardModifiers} onChange={event => setHidKeyboardModifiers(Number(event.target.value))} /></label>
          </div>}
          {(record.bluetooth_peers ?? []).map(peer => <div key={peer.peer_id} className="device-peer-row">
            <span>{peer.name} · {peer.kind === 'gatt' ? 'GATT' : peer.kind === 'hid_keyboard' ? `HID 键盘 · 已发送 ${peer.keyboard_reports_sent} 个报告` : peer.kind === 'beacon' ? 'Beacon' : `广播序列 ${peer.scripted_frame_count ?? 0} 帧${peer.repeat ? ' · 循环' : ''}`} · {peer.advertising ? '广播中' : '已停止'}</span>
          <div className="button-row">
            {peer.kind === 'hid_keyboard' && <><button type="button" disabled={!canUse('bluetooth') || !Number.isInteger(hidKeyboardUsage) || hidKeyboardUsage < 4 || hidKeyboardUsage > 231 || !Number.isInteger(hidKeyboardModifiers) || hidKeyboardModifiers < 0 || hidKeyboardModifiers > 255} onClick={() => send({ action: 'bluetooth_peer', parameters: { action: { command: 'send_hid_keyboard_report', peer_id: peer.peer_id, modifiers: hidKeyboardModifiers, keys: [hidKeyboardUsage] } } })}>按下</button><button type="button" disabled={!canUse('bluetooth')} onClick={() => send({ action: 'bluetooth_peer', parameters: { action: { command: 'send_hid_keyboard_report', peer_id: peer.peer_id, modifiers: 0, keys: [] } } })}>释放</button></>}
            <button type="button" disabled={!canUse('bluetooth') || peer.advertising} onClick={() => send({ action: 'bluetooth_peer', parameters: { action: { command: 'set_advertising', peer_id: peer.peer_id, enabled: true } } })}>广播</button>
            <button type="button" disabled={!canUse('bluetooth') || !peer.advertising} onClick={() => send({ action: 'bluetooth_peer', parameters: { action: { command: 'set_advertising', peer_id: peer.peer_id, enabled: false } } })}>停止广播</button>
            <button type="button" disabled={!canUse('bluetooth')} onClick={() => send({ action: 'bluetooth_peer', parameters: { action: { command: 'remove_peer', peer_id: peer.peer_id } } })}>移除</button>
          </div>
          </div>)}
          <div className="device-subsection">
            <strong>HCI 抓包</strong>
            <label><span>抓取时长 ms</span><input disabled={!canUse('bluetooth')} type="number" min={1000} max={30000} step={1000} value={bluetoothCaptureDurationMs} onChange={event => setBluetoothCaptureDurationMs(Number(event.target.value))} /></label>
            <button type="button" disabled={!canUse('bluetooth') || !Number.isInteger(bluetoothCaptureDurationMs) || bluetoothCaptureDurationMs < 1000 || bluetoothCaptureDurationMs > 30000} onClick={captureBluetoothHci}>开始限时抓包</button>
            {record.last_bluetooth_hci_capture && <p className="route-status">
              <strong>最近：{record.last_bluetooth_hci_capture.file_name}</strong>
              <span>{record.last_bluetooth_hci_capture.packets_captured} 包 · {record.last_bluetooth_hci_capture.output_size_bytes} 字节{record.last_bluetooth_hci_capture.packets_dropped ? ` · 丢弃 ${record.last_bluetooth_hci_capture.packets_dropped}` : ''}{record.last_bluetooth_hci_capture.truncated ? ' · 已截断' : ''}</span>
            </p>}
            <small>输出为 0600 btsnoop，最多 4 MiB，只记录当前实例 Guest↔Controller H4；“收集诊断”会安全打包该文件。</small>
          </div>
          <small>HID 键盘需先在 Android Bluetooth 设置中完成配对；仅在链路已加密且 Android 已订阅输入通知后发送报告。Beacon 最长 31 字节；广播序列最多 64 帧/10 分钟，不接受文件路径或任意 RootCanal 控制台。</small>
        </article>
        <article className="device-card">
          <h2><Nfc size={17} />NFC <CapabilityState enabled={enabled('nfc')} adbReady={record.adb_ready} capability={hostCapability('nfc')} loading={capabilitiesLoading} /></h2>
          <CapabilitySummary capability={hostCapability('nfc')} loading={capabilitiesLoading} />
          <label><span>NDEF hex</span><input disabled={!canUse('nfc')} className="mono-input" value={ndef} onChange={event => setNdef(event.target.value)} /></label>
          <div className="button-row">
            <button type="button" disabled={!canUse('nfc')} onClick={() => presentNfc('present_type2')}>放置 Type 2</button>
            <button type="button" disabled={!canUse('nfc')} onClick={() => presentNfc('present_type4')}>放置 Type 4</button>
            <button type="button" disabled={!canUse('nfc')} onClick={() => send({ action: 'nfc_tag', parameters: { action: { command: 'remove' } } })}>移除</button>
          </div>
        </article>
        <article className="device-card">
          <h2><Radio size={17} />UWB <CapabilityState enabled={enabled('uwb')} adbReady={record.adb_ready} capability={hostCapability('uwb')} loading={capabilitiesLoading} /></h2>
          <CapabilitySummary capability={hostCapability('uwb')} loading={capabilitiesLoading} />
          <label><span>Peer 距离 cm</span><input disabled={!canUse('uwb')} type="number" min={1} max={65535} step={1} value={uwbDistanceCm} onChange={event => setUwbDistanceCm(Number(event.target.value))} /></label>
          <button type="button" disabled={!canUse('uwb')} onClick={applyUwbRanging}>应用测距</button>
          <small>FiRa 会话生命周期仍由 Android 应用管理；会话测距中修改距离会立即发送新的 UCI 测量。</small>
        </article>
        <article className="device-card">
          <h2><Signal size={17} />蜂窝网络 <CapabilityState enabled={enabled('modem')} adbReady={record.adb_ready} capability={hostCapability('modem')} loading={capabilitiesLoading} /></h2>
          <CapabilitySummary capability={hostCapability('modem')} loading={capabilitiesLoading} />
          <label><span>运营商代码</span><input disabled={!canUse('modem')} inputMode="numeric" maxLength={6} value={modemOperatorNumeric} onChange={event => setModemOperatorNumeric(event.target.value)} /></label>
          <label><span>长名称</span><input disabled={!canUse('modem')} maxLength={32} value={modemOperatorLongName} onChange={event => setModemOperatorLongName(event.target.value)} /></label>
          <label><span>短名称</span><input disabled={!canUse('modem')} maxLength={16} value={modemOperatorShortName} onChange={event => setModemOperatorShortName(event.target.value)} /></label>
          <label><span>信号强度 0–31</span><input disabled={!canUse('modem')} type="number" min={0} max={31} step={1} value={modemSignalStrength} onChange={event => setModemSignalStrength(Number(event.target.value))} /></label>
          <label className="switch-line"><input disabled={!canUse('modem')} type="checkbox" checked={modemRegistered} onChange={event => setModemRegistered(event.target.checked)} /><span>已注册到网络</span></label>
          <button type="button" disabled={!canUse('modem')} onClick={applyModemState}>应用蜂窝状态</button>
          <small>状态按实例保存，并在 Guest 下一次 AT 查询时返回；不声明真实通话、短信、IMS 或移动数据接入。</small>
        </article>
        <article className="device-card passive-device-card">
          <h2><Volume2 size={17} />音频 <CapabilityState enabled={enabled('audio')} adbReady={record.adb_ready} capability={hostCapability('audio')} loading={capabilitiesLoading} /></h2>
          <CapabilitySummary capability={hostCapability('audio')} loading={capabilitiesLoading} />
          <p>播放与采集能力由 Guest virtio-snd 后端提供。</p>
        </article>
        <article className="device-card passive-device-card">
          <h2><Camera size={17} />相机 <CapabilityState enabled={enabled('camera')} adbReady={record.adb_ready} capability={hostCapability('camera')} loading={capabilitiesLoading} /></h2>
          <CapabilitySummary capability={hostCapability('camera')} loading={capabilitiesLoading} />
          <p>虚拟测试源、预览和 JPEG 拍摄由 Guest 相机后端提供。</p>
        </article>
      </div>
      <ConfirmDialog
        open={confirmNetworkSetup}
        title="安装 macOS 网络兼容服务？"
        description="HD 将请求管理员授权，安装受保护的 LaunchDaemon 并为全隧道 VPN 配置独立 PF anchor。安装失败会自动恢复原有系统配置。"
        confirmLabel="授权并安装"
        busy={busy}
        onCancel={() => setConfirmNetworkSetup(false)}
        onConfirm={() => {
          setConfirmNetworkSetup(false);
          post({ command: 'network_setup_install' });
        }}
      />
    </section>
  );
}

function DiagnosticsPage({
  record,
  status,
  artifactHint,
  bugreportHint,
  busy,
}: {
  record: InstanceRecord | null;
  status: string;
  artifactHint: string | null;
  bugreportHint: string | null;
  busy: boolean;
}) {
  const [includeGuestLogs, setIncludeGuestLogs] = useState(true);
  const [acknowledgeSensitiveData, setAcknowledgeSensitiveData] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [confirmPowerwash, setConfirmPowerwash] = useState(false);
  const [confirmPowerwashRestore, setConfirmPowerwashRestore] = useState(false);
  const [confirmPowerwashDiscard, setConfirmPowerwashDiscard] = useState(false);
  const [copyState, setCopyState] = useState<'idle' | 'copied' | 'failed'>('idle');
  const [bugreportCopyState, setBugreportCopyState] = useState<'idle' | 'copied' | 'failed'>('idle');
  const observed = record?.status.observed;
  const canDelete = Boolean(record && ['defined', 'stopped', 'failed', 'blocked'].includes(observed ?? ''));
  const stoppedAndroid = Boolean(record?.spec.guest_kind === 'android'
    && ['defined', 'stopped'].includes(observed ?? '')
    && !record.storage_transaction);
  const canPowerwash = Boolean(stoppedAndroid && !record?.powerwash_backup && (record?.frame_generation ?? 0) > 0);
  const canUsePowerwashBackup = Boolean(stoppedAndroid && record?.powerwash_backup);
  const canBugreport = Boolean(record?.spec.guest_kind === 'android' && observed === 'ready' && record.adb_ready);
  useEffect(() => {
    setAcknowledgeSensitiveData(false);
    setBugreportCopyState('idle');
    setConfirmPowerwash(false);
    setConfirmPowerwashRestore(false);
    setConfirmPowerwashDiscard(false);
  }, [record?.spec.id]);
  return (
    <section className="simple-page diagnostics-page">
      <h1>诊断</h1>
      <article className="diagnostic-card">
        <h2><FileArchive size={17} />诊断包</h2>
        <p>{status}</p>
        <label className="option-row">
          <input type="checkbox" disabled={!record} checked={includeGuestLogs && Boolean(record)} onChange={event => setIncludeGuestLogs(event.target.checked)} />
          <span>{record ? '包含 Guest 日志' : '未选择实例，将生成 Host-only 诊断包'}</span>
        </label>
        <button type="button" disabled={busy} onClick={() => post({ command: 'diagnostics', include_guest_logs: includeGuestLogs && Boolean(record) })}>
          {busy ? '正在生成…' : '生成诊断包'}
        </button>
        {artifactHint && (
          <div className="artifact-path">
            <code>{artifactHint}</code>
            <button type="button" aria-label="复制诊断包路径" onClick={async () => {
              setCopyState(await copyText(artifactHint) ? 'copied' : 'failed');
            }}>
              {copyState === 'copied' ? <Check size={15} /> : <Clipboard size={15} />}
              {copyState === 'copied' ? '已复制' : copyState === 'failed' ? '复制失败' : '复制'}
            </button>
            <button type="button" aria-label="在文件管理器中显示诊断包" onClick={() => post({ command: 'reveal_diagnostics' })}>
              <FolderOpen size={15} />
              定位
            </button>
          </div>
        )}
      </article>
      {record?.spec.guest_kind === 'android' && (
        <article className="diagnostic-card">
          <h2><FileArchive size={17} />Android bugreport</h2>
          <p>生成完整 AOSP dumpstate ZIP，可能需要数分钟，最大 256 MiB；只保留当前实例最近 3 份。</p>
          <label className="option-row">
            <input type="checkbox" checked={acknowledgeSensitiveData} onChange={event => setAcknowledgeSensitiveData(event.target.checked)} />
            <span>我了解该文件可能包含账号、网络、应用和设备敏感信息</span>
          </label>
          <button type="button" disabled={busy || !canBugreport || !acknowledgeSensitiveData} onClick={() => post({ command: 'android_bugreport' })}>
            {busy ? '正在生成…' : canBugreport ? '生成 Android bugreport' : '实例 Ready 且 ADB 可用后生成'}
          </button>
          {bugreportHint && (
            <div className="artifact-path">
              <code>{bugreportHint}</code>
              <button type="button" aria-label="复制 Android bugreport 路径" onClick={async () => {
                setBugreportCopyState(await copyText(bugreportHint) ? 'copied' : 'failed');
              }}>
                {bugreportCopyState === 'copied' ? <Check size={15} /> : <Clipboard size={15} />}
                {bugreportCopyState === 'copied' ? '已复制' : bugreportCopyState === 'failed' ? '复制失败' : '复制'}
              </button>
              <button type="button" aria-label="在文件管理器中显示 Android bugreport" onClick={() => post({ command: 'reveal_android_bugreport' })}>
                <FolderOpen size={15} />
                定位
              </button>
            </div>
          )}
        </article>
      )}
      {record?.spec.guest_kind === 'android' && (
        <article className="diagnostic-card danger-zone">
          <h2><RotateCcw size={17} />恢复出厂数据</h2>
          <p>仅在实例关闭后执行。原 userdata 会原子移动为私有备份；不会复制大型镜像，也不会删除实例设置。</p>
          {record.powerwash_backup ? (
            <div className="powerwash-backup">
              <strong>可恢复备份</strong>
              <span>{formatBytes(record.powerwash_backup.size_bytes)} · {new Date(record.powerwash_backup.created_at).toLocaleString()}</span>
              <code>{record.powerwash_backup.sha256}</code>
              <div className="inline-actions">
                <button type="button" disabled={busy || !canUsePowerwashBackup} onClick={() => setConfirmPowerwashRestore(true)}>恢复旧数据</button>
                <button type="button" className="danger-button" disabled={busy || !canUsePowerwashBackup} onClick={() => setConfirmPowerwashDiscard(true)}>永久丢弃备份</button>
              </div>
            </div>
          ) : (
            <button type="button" disabled={busy || !canPowerwash} onClick={() => setConfirmPowerwash(true)}>
              {stoppedAndroid
                ? (record.frame_generation > 0 ? '恢复出厂数据并保留备份' : '实例尚未创建 userdata')
                : '必须先关闭实例'}
            </button>
          )}
        </article>
      )}
      <article className="diagnostic-card danger-zone">
        <h2><Trash2 size={17} />删除实例</h2>
        <p>{canDelete ? '删除实例、磁盘和运行记录；此操作不可撤销。' : '必须先关闭实例，才能执行删除。'}</p>
        <button type="button" disabled={busy || !canDelete} onClick={() => setConfirmDelete(true)}>删除当前实例</button>
      </article>
      <ConfirmDialog
        open={confirmPowerwash}
        title={`恢复“${record?.spec.name ?? ''}”的出厂数据？`}
        description="当前 Android userdata 将移入 owner-only 备份。下一次启动会创建干净 userdata；实例配置保持不变。"
        confirmLabel="备份并恢复出厂数据"
        requiredText={record?.spec.name ?? ''}
        danger
        busy={busy}
        onCancel={() => setConfirmPowerwash(false)}
        onConfirm={() => {
          setConfirmPowerwash(false);
          if (record) post({
            command: 'operation',
            kind: 'powerwash',
            expected_revision: record.status.revision,
            confirmation_name: record.spec.name,
          });
        }}
      />
      <ConfirmDialog
        open={confirmPowerwashRestore}
        title={`恢复“${record?.spec.name ?? ''}”的旧数据？`}
        description="当前 userdata 会先成为新的回滚备份，再恢复所选旧数据，因此本次恢复仍可撤销。"
        confirmLabel="恢复旧数据"
        requiredText={record?.spec.name ?? ''}
        danger
        busy={busy}
        onCancel={() => setConfirmPowerwashRestore(false)}
        onConfirm={() => {
          setConfirmPowerwashRestore(false);
          if (record?.powerwash_backup) post({
            command: 'operation',
            kind: 'restore_powerwash',
            backup_id: record.powerwash_backup.id,
            expected_revision: record.status.revision,
            confirmation_name: record.spec.name,
          });
        }}
      />
      <ConfirmDialog
        open={confirmPowerwashDiscard}
        title={`永久丢弃“${record?.spec.name ?? ''}”的数据备份？`}
        description="该备份文件将被永久删除且不可恢复；当前 userdata 不受影响。"
        confirmLabel="永久丢弃备份"
        requiredText={record?.spec.name ?? ''}
        danger
        busy={busy}
        onCancel={() => setConfirmPowerwashDiscard(false)}
        onConfirm={() => {
          setConfirmPowerwashDiscard(false);
          if (record?.powerwash_backup) post({
            command: 'operation',
            kind: 'discard_powerwash_backup',
            backup_id: record.powerwash_backup.id,
            expected_revision: record.status.revision,
            confirmation_name: record.spec.name,
          });
        }}
      />
      <ConfirmDialog
        open={confirmDelete}
        title={`永久删除“${record?.spec.name ?? ''}”？`}
        description="实例磁盘、配置和运行记录将被删除，且无法恢复。"
        confirmLabel="永久删除"
        danger
        busy={busy}
        onCancel={() => setConfirmDelete(false)}
        onConfirm={() => {
          setConfirmDelete(false);
          post({ command: 'operation', kind: 'delete' });
        }}
      />
    </section>
  );
}

interface LayoutState {
  page: Page;
  sidebar_collapsed: boolean;
  apk_path: string;
  location_route_path: string;
  display_maximized: boolean;
  window_expanded: boolean;
  android_focused: boolean;
  sidebar_visible: boolean;
}

const emptyLayout: LayoutState = {
  page: 'player',
  sidebar_collapsed: true,
  apk_path: '',
  location_route_path: '',
  display_maximized: false,
  window_expanded: false,
  android_focused: false,
  sidebar_visible: false,
};

function useHostState() {
  const [snapshot, setSnapshot] = useState(emptySnapshot);
  const [layout, setLayout] = useState(emptyLayout);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState('');

  useEffect(() => {
    window.__hdReceive = (message: HostMessage) => {
      if (message.type === 'snapshot') setSnapshot(message.payload as Snapshot);
      if (message.type === 'titlebar') {
        setSnapshot(current => ({ ...current, titlebar: message.payload as Snapshot['titlebar'] }));
      }
      if (message.type === 'layout') {
        setLayout(current => ({ ...current, ...(message.payload as Partial<LayoutState>) }));
      }
      if (message.type === 'busy') setBusy(Boolean(message.payload));
      if (message.type === 'notice') {
        setNotice(String(message.payload ?? ''));
      }
    };
    const surface = new URLSearchParams(window.location.search).get('surface') ?? 'content';
    post({ command: 'ready', surface });
    return () => { delete window.__hdReceive; };
  }, []);

  return { snapshot, layout, busy, notice };
}

function SidebarSurface() {
  const { snapshot, layout, busy, notice } = useHostState();
  const [query, setQuery] = useState('');
  const [creating, setCreating] = useState(false);
  const [newName, setNewName] = useState('Android');
  const [newKind, setNewKind] = useState<'android' | 'microdroid'>('android');
  const [powerMenu, setPowerMenu] = useState<{ x: number; y: number; instanceId: string } | null>(null);
  const filtered = snapshot.summaries.filter(item => item.name.toLowerCase().includes(query.toLowerCase()));
  const selectedId = snapshot.selected?.spec.id;
  const runtimeDisplays = snapshot.selected?.spec.guest_kind === 'android'
    ? snapshot.selected.runtime_displays
    : [];
  const selectedDisplayId = snapshot.selected_display.kind === 'primary'
    ? 'primary'
    : snapshot.selected_display.id;
  const observed = snapshot.selected?.status.observed ?? '';
  const microdroidSelected = snapshot.selected?.spec.guest_kind === 'microdroid';
  const transitional = transitionStates.has(observed);
  const menuTargetReady = Boolean(powerMenu && selectedId === powerMenu.instanceId);
  const canStart = menuTargetReady && !busy && !transitional && ['defined', 'stopped', 'failed', 'blocked'].includes(observed);
  const canPause = !microdroidSelected && menuTargetReady && !busy && observed === 'ready';
  const canResume = !microdroidSelected && menuTargetReady && !busy && observed === 'paused';
  const safePowerControl = Boolean(snapshot.selected?.adb_ready);
  const canRestart = menuTargetReady && safePowerControl && !busy && ['ready', 'paused'].includes(observed);
  const canStop = menuTargetReady && !busy && ['ready', 'paused'].includes(observed)
    && (microdroidSelected || safePowerControl);
  const sidebarNotice = ['ready', 'paused'].includes(observed) && !safePowerControl
    ? microdroidSelected
      ? '当前 Payload 没有可用的 ADB 关机通道；重启不可用，停止将明确执行强制终止'
      : 'ADB 控制未就绪；已禁用关机与重启以保护实例数据'
    : notice;

  useEffect(() => {
    const closeOnBlur = () => post({ command: 'close_sidebar' });
    window.addEventListener('blur', closeOnBlur);
    return () => window.removeEventListener('blur', closeOnBlur);
  }, []);

  const cancelCreate = () => {
    setCreating(false);
    setNewName('Android');
    setNewKind('android');
  };
  const submitCreate = () => {
    const name = newName.trim();
    if (!name || busy) return;
    post({ command: 'create', name, guest_kind: newKind });
    cancelCreate();
  };
  const navigate = (page: Page) => post({ command: 'page', page });
  const operation = (kind: string) => {
    const instanceId = powerMenu?.instanceId;
    setPowerMenu(null);
    if (instanceId) post({ command: 'operation', kind, instance_id: instanceId });
  };
  const openPowerMenu = (event: React.MouseEvent<HTMLElement>, instanceId?: string) => {
    event.preventDefault();
    event.stopPropagation();
    if (!instanceId) return;
    if (selectedId !== instanceId) post({ command: 'select', instance_id: instanceId });
    const width = 184;
    const height = 185;
    setPowerMenu({
      x: Math.max(8, Math.min(event.clientX, window.innerWidth - width - 8)),
      y: Math.max(8, Math.min(event.clientY, window.innerHeight - height - 8)),
      instanceId,
    });
  };

  useEffect(() => {
    if (!powerMenu) return undefined;
    const dismiss = (event: PointerEvent) => {
      if (!(event.target as HTMLElement).closest('.sidebar-context-menu')) setPowerMenu(null);
    };
    const dismissKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setPowerMenu(null);
    };
    const dismissBlur = () => setPowerMenu(null);
    window.addEventListener('pointerdown', dismiss);
    window.addEventListener('keydown', dismissKey);
    window.addEventListener('blur', dismissBlur, { once: true });
    return () => {
      window.removeEventListener('pointerdown', dismiss);
      window.removeEventListener('keydown', dismissKey);
      window.removeEventListener('blur', dismissBlur);
    };
  }, [powerMenu]);

  return (
    <aside className="sidebar surface-sidebar">
      <div className="sidebar-top">
        <button type="button" className="new-button" aria-expanded={creating} onClick={() => {
          if (creating) cancelCreate();
          else {
            setNewName('Android');
            setNewKind('android');
            setCreating(true);
          }
        }}>
          {creating ? <X size={18} /> : <Plus size={18} />}
          <span>{creating ? '取消新建' : '新建实例'}</span>
        </button>
        {creating && (
          <form className="create-row" onSubmit={event => { event.preventDefault(); submitCreate(); }}>
            <select
              aria-label="实例类型"
              value={newKind}
              onChange={event => {
                const kind = event.target.value as 'android' | 'microdroid';
                setNewKind(kind);
                setNewName(kind === 'android' ? 'Android' : 'Microdroid');
              }}
            >
              <option value="android">Android</option>
              {snapshot.microdroid_supported && <option value="microdroid">Microdroid</option>}
            </select>
            <input
              autoFocus
              value={newName}
              aria-label="实例名称"
              onChange={event => setNewName(event.target.value)}
              onKeyDown={event => { if (event.key === 'Escape') cancelCreate(); }}
            />
            <button type="submit" aria-label="创建实例" title="创建实例" disabled={busy || !newName.trim()}><Play size={16} /></button>
            <button type="button" className="cancel-create" aria-label="取消新建" title="取消" disabled={busy} onClick={cancelCreate}><X size={16} /></button>
          </form>
        )}
        <div className="search">
          <Search size={16} />
          <input value={query} onChange={event => setQuery(event.target.value)} placeholder="搜索实例" aria-label="搜索实例" />
          {query && <button type="button" aria-label="清除搜索" onClick={() => setQuery('')}><X size={14} /></button>}
        </div>
      </div>
      <div className="sidebar-label"><span>实例</span><span>{filtered.length}</span></div>
      <div className="instance-list">
        {filtered.map(item => (
          <InstanceRow
            key={item.id}
            item={item}
            selected={selectedId === item.id}
            onSelect={() => {
              if (selectedId !== item.id) {
                post({ command: 'select', instance_id: item.id });
              }
              navigate('player');
            }}
            onMenu={event => openPowerMenu(event, item.id)}
          />
        ))}
        {filtered.length === 0 && (
          <div className="sidebar-empty">{query ? '没有匹配的实例' : '还没有实例'}</div>
        )}
      </div>
      {runtimeDisplays.length > 1 && (
        <section className="display-switcher" aria-labelledby="display-switcher-label">
          <div className="display-switcher-heading" id="display-switcher-label">
            <span><MonitorCog size={14} />显示器</span>
            <small>{runtimeDisplays.length}</small>
          </div>
          <div className="display-switcher-options" role="group" aria-label="Player 显示器">
            {runtimeDisplays.map(display => {
              const displayId = display.display_id.kind === 'primary'
                ? 'primary'
                : display.display_id.id;
              const selected = displayId === selectedDisplayId;
              return (
                <button
                  type="button"
                  key={displayId}
                  className={selected ? 'selected' : ''}
                  aria-pressed={selected}
                  disabled={busy}
                  title={`${display.name} · ${display.width} × ${display.height}`}
                  onClick={() => {
                    if (!selected) post({ command: 'select_display', display_id: displayId });
                  }}
                >
                  <span>{display.name}</span>
                  <small>{display.width} × {display.height}</small>
                </button>
              );
            })}
          </div>
        </section>
      )}
      <NoticeBanner message={sidebarNotice} busy={busy} />
      <nav className="sidebar-nav" aria-label="主要页面">
        <button type="button" className={layout.page === 'player' ? 'active' : ''} onClick={() => navigate('player')}><AppWindow size={18} /><span>{snapshot.selected?.spec.guest_kind === 'microdroid' ? '工作负载' : 'Player'}</span></button>
        <button type="button" className={layout.page === 'settings' ? 'active' : ''} onClick={() => navigate('settings')}><Settings size={18} /><span>设置</span></button>
        {snapshot.selected?.spec.guest_kind !== 'microdroid' && <button type="button" className={layout.page === 'devices' ? 'active' : ''} onClick={() => navigate('devices')}><Boxes size={18} /><span>设备</span></button>}
        <button type="button" className={layout.page === 'diagnostics' ? 'active' : ''} onClick={() => navigate('diagnostics')}><Gauge size={18} /><span>诊断</span></button>
      </nav>
      {powerMenu && (
        <div className="sidebar-context-menu" role="menu" style={{ left: powerMenu.x, top: powerMenu.y }} onContextMenu={event => event.preventDefault()}>
          <button type="button" role="menuitem" disabled={!canStart} onClick={() => operation('start')}><Play size={16} />启动</button>
          {!microdroidSelected && <button type="button" role="menuitem" disabled={observed === 'paused' ? !canResume : !canPause} onClick={() => operation(observed === 'paused' ? 'resume' : 'pause')}>
            <CirclePause size={16} />{observed === 'paused' ? '恢复' : '暂停'}
          </button>}
          <div className="context-menu-separator" />
          <button type="button" role="menuitem" disabled={!canRestart} onClick={() => operation('restart')}><RefreshCw size={16} />重启</button>
          <button
            type="button"
            role="menuitem"
            className="danger"
            disabled={!canStop}
            onClick={() => operation(microdroidSelected && !safePowerControl ? 'force_stop' : 'stop')}
          >
            <Power size={16} />{microdroidSelected && !safePowerControl ? '强制停止' : '关机'}
          </button>
        </div>
      )}
    </aside>
  );
}

function ContentSurface() {
  const { snapshot, layout, busy, notice } = useHostState();
  const [apkPath, setApkPath] = useState(layout.apk_path);
  const [locationRoutePath, setLocationRoutePath] = useState(layout.location_route_path);
  useEffect(() => setApkPath(layout.apk_path), [layout.apk_path]);
  useEffect(() => setLocationRoutePath(layout.location_route_path), [layout.location_route_path]);
  const updateApkPath = (value: string) => {
    setApkPath(value);
    post({ command: 'set_apk_path', path: value });
  };
  const updateLocationRoutePath = (value: string) => {
    setLocationRoutePath(value);
    post({ command: 'set_location_route_path', path: value });
  };
  return (
    <main className="main-panel surface-content">
      {!snapshot.host_runtime_current && (
        <div className="runtime-upgrade-banner" role="alert">
          <AlertCircle size={16} />
          <span>旧版 HD 运行时仍在承载实例。停止所有实例并重新打开 HD 即可完成升级。</span>
          <button type="button" onClick={() => post({ command: 'toggle_sidebar' })}>查看实例</button>
        </div>
      )}
      {layout.page === 'player' && snapshot.selected?.spec.guest_kind === 'microdroid' && (
        <section className="simple-page microdroid-workload">
          <div className="page-heading">
            <div>
              <h1>Microdroid 工作负载</h1>
              <p>无图形虚拟机；运行状态、Payload 输出、ADB 与控制台属于当前实例。</p>
            </div>
            <span className={`adb-badge ${snapshot.selected.adb_ready ? 'ready' : ''}`}>{stateLabel(snapshot.selected.status.observed)}</span>
          </div>
          <div className="device-grid">
            <article className="device-card">
              <h2><FileArchive size={17} />Payload</h2>
              <p>{snapshot.selected.spec.microdroid?.payload.kind === 'uploaded' ? '实例上传的 Payload APK' : '内置 EmptyPayload'}</p>
              <small>{snapshot.selected.spec.microdroid?.payload.kind === 'uploaded'
                ? '配置：APK 内 assets/vm_config.json'
                : '配置：系统内置 Microdroid EmptyPayload'}</small>
              <button type="button" disabled={isActive(snapshot.selected) || busy} onClick={() => post({ command: 'choose_microdroid_payload' })}>选择 Payload APK…</button>
            </article>
            <article className="device-card">
              <h2><AppWindow size={17} />控制台与 ADB</h2>
              <p>{snapshot.selected.adb_ready
                ? `ADB 已就绪：${snapshot.selected.adb_serial ?? '实例端口'}`
                : snapshot.selected.status.observed === 'ready'
                  ? '当前 Payload 未提供可用的 adbd 服务'
                  : '等待 Guest 调试通道'}</p>
              <button type="button" disabled={!snapshot.selected.adb_ready || busy} onClick={() => post({ command: 'microdroid_shell' })}>打开 Shell</button>
            </article>
          </div>
        </section>
      )}
      {layout.page === 'settings' && (
        <SettingsPage
          record={snapshot.selected}
          busy={busy}
          apkPath={apkPath}
          setApkPath={updateApkPath}
          onChooseApk={() => post({ command: 'choose_apk' })}
          onInstallApk={() => post({ command: 'install_apk', path: apkPath })}
          onChoosePayload={() => post({ command: 'choose_microdroid_payload' })}
          onSave={(spec, restart, expectedRevision) => post({
            command: 'save_spec',
            spec,
            restart,
            expected_revision: expectedRevision,
          })}
          deviceCapabilities={snapshot.device_capabilities}
          capabilitiesLoading={snapshot.device_capabilities_loading}
          resourceCapability={snapshot.resource_capability}
          resourceCapabilityLoading={snapshot.resource_capability_loading}
        />
      )}
      {layout.page === 'devices' && (
        <DevicesPage
          record={snapshot.selected}
          busy={busy}
          locationRoutePath={locationRoutePath}
          setLocationRoutePath={updateLocationRoutePath}
          deviceCapabilities={snapshot.device_capabilities}
          capabilitiesLoading={snapshot.device_capabilities_loading}
          networkSetup={snapshot.network_setup}
        />
      )}
      {layout.page === 'diagnostics' && (
        <DiagnosticsPage
          record={snapshot.selected}
          status={snapshot.status}
          artifactHint={snapshot.diagnostic_artifact}
          bugreportHint={snapshot.android_bugreport_artifact}
          busy={busy}
        />
      )}
      <div className="content-notice"><NoticeBanner message={notice} busy={busy} /></div>
    </main>
  );
}

function SurfaceApp() {
  const surface = new URLSearchParams(window.location.search).get('surface');
  if (surface === 'top') return <TopSurface />;
  if (surface === 'sidebar') return <SidebarSurface />;
  return <ContentSurface />;
}

createRoot(document.getElementById('root')!).render(
  <React.StrictMode><SurfaceApp /></React.StrictMode>,
);

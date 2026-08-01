import React, { useEffect, useMemo, useState } from 'react';
import { createRoot } from 'react-dom/client';
import * as Tooltip from '@radix-ui/react-tooltip';
import {
  Activity, AlertCircle, AppWindow, ArrowLeft, Battery, Bluetooth, Boxes, Camera,
  Check, ChevronDown, CirclePause, Clipboard, Expand, FileArchive, Gauge,
    HardDrive, Home, MapPin, Minus, MonitorCog, MoreHorizontal, Nfc,
    Minimize2, PackagePlus, PanelLeftClose, PanelLeftOpen, Play, Plus, Power, Radio, RefreshCw, RotateCw, Search, Settings, Signal,
  SlidersHorizontal, Square, SquareStack, Trash2, Volume1, Volume2, Wifi, X,
} from 'lucide-react';
import type { LucideIcon } from 'lucide-react';
import { post, type HostMessage } from './bridge';
import type { DeviceCapability, InstanceRecord, InstanceSpec, InstanceSummary, Snapshot } from './types';
import './style.css';

type Page = 'player' | 'settings' | 'devices' | 'diagnostics';
type IconType = LucideIcon;

const isMacOS = /Macintosh|Mac OS X/.test(navigator.userAgent);

const emptySnapshot: Snapshot = {
  summaries: [],
  selected: null,
  status: '正在连接 HD Host…',
  artifact_hint: null,
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

function androidActionsReady(record: InstanceRecord | null) {
  return Boolean(record && record.status.observed === 'ready' && record.adb_ready);
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

function ToolButton({
  label,
  icon: Icon,
  onClick,
  disabled = false,
}: {
  label: string;
  icon: IconType;
  onClick: () => void;
  disabled?: boolean;
}) {
  return (
    <Tooltip.Root delayDuration={350}>
      <Tooltip.Trigger asChild>
        <button
          type="button"
          className="icon-button"
          aria-label={label}
          disabled={disabled}
          onClick={onClick}
        >
          <Icon size={19} strokeWidth={1.8} />
        </button>
      </Tooltip.Trigger>
      <Tooltip.Portal>
        <Tooltip.Content className="tooltip" sideOffset={8}>{label}</Tooltip.Content>
      </Tooltip.Portal>
    </Tooltip.Root>
  );
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
  danger = false,
  busy = false,
  onCancel,
  onConfirm,
}: {
  open: boolean;
  title: string;
  description: string;
  confirmLabel: string;
  danger?: boolean;
  busy?: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
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
        <div className="dialog-actions">
          <button type="button" disabled={busy} onClick={onCancel}>取消</button>
          <button
            type="button"
            className={danger ? 'danger-button' : 'primary-button'}
            disabled={busy}
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
            {stateLabel(item.status.observed)}
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

function PlayerTools({
  record,
  busy,
  apkPath,
  displayMaximized,
}: {
  record: InstanceRecord | null;
  busy: boolean;
  apkPath: string;
  displayMaximized: boolean;
}) {
  const action = (command: string, extra: Record<string, unknown> = {}) => post({ command, ...extra });
  const actionReady = androidActionsReady(record);
  const powerReady = Boolean(record && record.status.observed === 'ready');
  return (
    <div className="tool-group">
      <ToolButton label="电源" icon={Power} onClick={() => action('key', { key: 'power' })} disabled={!powerReady || busy} />
      <ToolButton label="音量减" icon={Volume1} onClick={() => action('key', { key: 'volume_down' })} disabled={!actionReady || busy} />
      <ToolButton label="音量加" icon={Volume2} onClick={() => action('key', { key: 'volume_up' })} disabled={!actionReady || busy} />
      <ToolButton label="旋转" icon={RotateCw} onClick={() => action('rotate')} disabled={!actionReady || busy} />
      <ToolButton label="截图" icon={Camera} onClick={() => action('screenshot')} disabled={!actionReady || busy} />
      <ToolButton
        label={apkPath ? '安装已选 APK' : '选择并安装 APK'}
        icon={PackagePlus}
        onClick={() => action(apkPath ? 'install_apk' : 'choose_install_apk', apkPath ? { path: apkPath } : {})}
        disabled={!actionReady || busy}
      />
      <ToolButton
        label={displayMaximized ? '退出显示最大化' : '最大化显示区域'}
        icon={displayMaximized ? Minimize2 : Expand}
        onClick={() => action('window', { action: 'maximize_display' })}
        disabled={!record}
      />
      <span className="tool-divider" />
      <ToolButton label="最近任务" icon={SquareStack} onClick={() => action('key', { key: 'recent' })} disabled={!actionReady || busy} />
      <ToolButton label="主页" icon={Home} onClick={() => action('key', { key: 'home' })} disabled={!actionReady || busy} />
      <ToolButton label="返回" icon={ArrowLeft} onClick={() => action('key', { key: 'back' })} disabled={!actionReady || busy} />
    </div>
  );
}

const settingSections = [
  ['general', '常规', Settings],
  ['performance', '性能', Gauge],
  ['display', '显示', MonitorCog],
  ['adb', 'Android / ADB', AppWindow],
  ['artifacts', '制品与启动', HardDrive],
  ['devices', '设备模拟', Boxes],
  ['advanced', '高级', SlidersHorizontal],
] as const;

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
  unsupportedDevices.forEach(key => {
    if (key in spec.devices) spec.devices[key] = false;
  });
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

function validateSpec(spec: InstanceSpec) {
  const errors: Record<string, string> = {};
  if (!spec.name.trim()) errors.name = '实例名称不能为空';
  if (!Number.isInteger(spec.cpu_count) || spec.cpu_count < 1 || spec.cpu_count > 256) errors.cpu = '请输入 1–256';
  if (!Number.isInteger(spec.memory_mib) || spec.memory_mib < 2048 || spec.memory_mib > 1048576) errors.memory = '请输入 2048–1048576 MiB';
  if (!Number.isInteger(spec.display.width) || spec.display.width < 320 || spec.display.width > 8192) errors.width = '请输入 320–8192';
  if (!Number.isInteger(spec.display.height) || spec.display.height < 320 || spec.display.height > 8192) errors.height = '请输入 320–8192';
  if (!Number.isInteger(spec.display.dpi) || spec.display.dpi < 72 || spec.display.dpi > 960) errors.dpi = '请输入 72–960';
  if (!Number.isInteger(spec.boot.kernel_log_level) || spec.boot.kernel_log_level < 0 || spec.boot.kernel_log_level > 7) errors.logLevel = '请输入 0–7';
  if (!Number.isInteger(spec.boot.panic_timeout_seconds) || spec.boot.panic_timeout_seconds < 0 || spec.boot.panic_timeout_seconds > 300) errors.panic = '请输入 0–300 秒';
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
  onSave,
  deviceCapabilities,
  capabilitiesLoading,
}: {
  record: InstanceRecord | null;
  busy: boolean;
  apkPath: string;
  setApkPath: (path: string) => void;
  onChooseApk: () => void;
  onInstallApk: () => void;
  onSave: (spec: InstanceSpec, restart: boolean) => void;
  deviceCapabilities: DeviceCapability[];
  capabilitiesLoading: boolean;
}) {
  const capabilityMap = useMemo(
    () => new Map(deviceCapabilities.map(capability => [capability.id, capability])),
    [deviceCapabilities],
  );
  const unsupportedDevices = useMemo(
    () => deviceCapabilities.filter(capability => !capability.available).map(capability => capability.id),
    [deviceCapabilities],
  );
  const [section, setSection] = useState<(typeof settingSections)[number][0]>('general');
  const unsupportedKey = unsupportedDevices.join('\0');
  const [draft, setDraft] = useState<InstanceSpec | null>(() => editableSpec(record, unsupportedDevices));
  const [confirmRestart, setConfirmRestart] = useState(false);
  const specKey = record ? JSON.stringify(record.spec) : '';

  useEffect(() => {
    setDraft(editableSpec(record, unsupportedDevices));
    setConfirmRestart(false);
  }, [record?.spec.id, specKey, unsupportedKey]);

  if (!record || !draft) {
    return <section className="empty-page"><h1>设置</h1><p>请先选择一个实例。</p></section>;
  }

  const mutate = (fn: (next: InstanceSpec) => void) => {
    const next = structuredClone(draft);
    fn(next);
    setDraft(next);
  };
  const errors = validateSpec(draft);
  const dirty = JSON.stringify(draft) !== JSON.stringify(record.spec);
  const active = isActive(record);
  const valid = Object.keys(errors).length === 0;
  const restartSafe = !active || record.adb_ready;
  const canSave = dirty && valid && !busy && restartSafe;
  const save = () => {
    if (!canSave) return;
    if (active) setConfirmRestart(true);
    else onSave(draft, false);
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
        <div className="content-title"><Settings size={19} /><span>设置</span></div>
        <button
          type="button"
          className="primary-button"
          disabled={!canSave}
          title={!restartSafe ? 'ADB 未就绪；为保护实例数据，运行中不能保存并重启' : undefined}
          onClick={save}
        >
          {busy ? '正在保存…' : active ? '保存并重启' : '保存更改'}
        </button>
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
          {active && !record.adb_ready && (
            <div className="inline-callout warning">
              ADB 控制尚未就绪。为避免强制关机损坏实例数据，运行中不能保存并重启。
            </div>
          )}
          {dirty && active && record.adb_ready && (
            <div className="inline-callout warning">
              当前实例正在运行。保存后 HD 将安全停止并重新启动 Android。
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
              <NumberField label="处理器" value={draft.cpu_count} min={1} max={256} suffix="核" error={errors.cpu} onChange={value => mutate(next => { next.cpu_count = value; })} />
              <NumberField label="内存" value={draft.memory_mib} min={2048} max={1048576} suffix="MiB" error={errors.memory} onChange={value => mutate(next => { next.memory_mib = value; })} />
            </>
          )}
          {section === 'display' && (
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
            </>
          )}
          {section === 'adb' && (
            <>
              <h2>Android / ADB</h2>
              <label className="setting-row">
                <span>ADB</span>
                <select value={draft.adb.mode} onChange={event => mutate(next => { next.adb.mode = event.target.value as 'disabled' | 'loopback'; })}>
                  <option value="loopback">本机回环</option>
                  <option value="disabled">禁用</option>
                </select>
              </label>
              <label className="setting-row">
                <span>ADB 可执行文件</span>
                <input className="mono-input" value={draft.adb.executable ?? ''} placeholder="自动发现" onChange={event => mutate(next => { next.adb.executable = event.target.value || null; })} />
              </label>
              <label className="setting-row">
                <span>APK 文件</span>
                <span className="apk-picker">
                  <input className="mono-input" value={apkPath} placeholder="尚未选择 APK" onChange={event => setApkPath(event.target.value)} />
                  <button type="button" className="secondary-button" disabled={busy} onClick={onChooseApk}>选择…</button>
                </span>
              </label>
              <div className="setting-action-row">
                <button type="button" className="primary-button" disabled={busy || !androidActionsReady(record) || !apkPath} onClick={onInstallApk}>
                  <PackagePlus size={16} />安装 APK
                </button>
              </div>
              <p className="section-description">可使用系统文件选择器，也可以将 APK 文件直接拖入 HD 窗口。</p>
            </>
          )}
          {section === 'artifacts' && (
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
              <NumberField label="内核日志级别" value={draft.boot.kernel_log_level} min={0} max={7} error={errors.logLevel} onChange={value => mutate(next => { next.boot.kernel_log_level = value; })} />
              <label className="setting-row">
                <span>开机动画</span>
                <input type="checkbox" checked={draft.boot.boot_animation} onChange={event => mutate(next => { next.boot.boot_animation = event.target.checked; })} />
              </label>
            </>
          )}
          {section === 'devices' && (
            <>
              <h2>设备模拟</h2>
              {Object.entries(draft.devices).map(([key, enabled]) => {
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
                      onChange={event => mutate(next => { next.devices[key] = event.target.checked; })}
                    />
                    {disabled && <span className="sr-only" id={`unsupported-${key}`}>{capabilitiesLoading ? '正在检测当前主机设备能力' : '当前主机不支持此设备模拟'}</span>}
                  </label>
                );
              })}
            </>
          )}
          {section === 'advanced' && (
            <>
              <h2>高级</h2>
              <NumberField label="Kernel log level" value={draft.boot.kernel_log_level} min={0} max={7} error={errors.logLevel} onChange={value => mutate(next => { next.boot.kernel_log_level = value; })} />
              <NumberField label="Panic timeout" value={draft.boot.panic_timeout_seconds} min={0} max={300} suffix="秒" error={errors.panic} onChange={value => mutate(next => { next.boot.panic_timeout_seconds = value; })} />
              <label className="setting-row">
                <span>显示 Host FPS</span>
                <input type="checkbox" checked={draft.display.show_host_fps} onChange={event => mutate(next => { next.display.show_host_fps = event.target.checked; })} />
              </label>
            </>
          )}
        </div>
      </div>
      <ConfirmDialog
        open={confirmRestart}
        title="保存设置并重启 Android？"
        description="HD 将安全停止当前实例、保存全部更改，然后重新启动。未保存的 Guest 数据可能丢失。"
        confirmLabel="保存并重启"
        busy={busy}
        onCancel={() => setConfirmRestart(false)}
        onConfirm={() => {
          setConfirmRestart(false);
          onSave(draft, true);
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
  return (
    <span className={`capability-state ${available && enabled && adbReady ? 'available' : ''}`}>
      {loading
        ? '检测中'
        : !capability
          ? '能力未知'
          : !available
            ? '当前主机不支持'
              : !enabled
                ? '实例未启用'
                : adbReady
                  ? capability.backend === 'software_backed' ? '模拟可用' : '可用'
                  : '等待 ADB'}
    </span>
  );
}

function CapabilitySummary({ capability, loading }: { capability?: DeviceCapability; loading: boolean }) {
  if (loading) return <p className="device-capability-summary">正在检测当前主机设备能力…</p>;
  if (!capability) return <p className="device-capability-summary">当前主机未返回该设备的能力信息。</p>;
  return (
    <p className="device-capability-summary" title={capability.boundary}>
      <strong>{deviceBackendLabels[capability.backend]}</strong>
      <span>{capability.features.length ? capability.features.join(' · ') : '无可声明功能'}</span>
      <small>{capability.boundary}</small>
    </p>
  );
}

function DevicesPage({
  record,
  busy,
  deviceCapabilities,
  capabilitiesLoading,
}: {
  record: InstanceRecord | null;
  busy: boolean;
  deviceCapabilities: DeviceCapability[];
  capabilitiesLoading: boolean;
}) {
  const [latitude, setLatitude] = useState('37.4219999');
  const [longitude, setLongitude] = useState('-122.0840577');
  const [battery, setBattery] = useState(100);
  const [charging, setCharging] = useState(true);
  const [latency, setLatency] = useState(0);
  const [lossPercent, setLossPercent] = useState(0);
  const [sensor, setSensor] = useState('accelerometer');
  const [sensorValues, setSensorValues] = useState('0,0,9806650');
  const [peerName, setPeerName] = useState('HD GATT peer');
  const [peerCreated, setPeerCreated] = useState(false);
  const [ndef, setNdef] = useState('D1010B5402656E48656C6C6F204844');
  const [formError, setFormError] = useState('');
  const peerId = useMemo(() => crypto.randomUUID(), []);
  const capabilityMap = useMemo(
    () => new Map(deviceCapabilities.map(capability => [capability.id, capability])),
    [deviceCapabilities],
  );

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
      && Boolean(capability?.features.includes('runtime_control'));
  };
  const send = (action: Record<string, unknown>) => {
    setFormError('');
    post({ command: 'action', action });
  };
  const applyLocation = () => {
    const lat = Number(latitude);
    const lon = Number(longitude);
    if (!Number.isFinite(lat) || lat < -90 || lat > 90 || !Number.isFinite(lon) || lon < -180 || lon > 180) {
      setFormError('纬度应为 -90–90，经度应为 -180–180。');
      return;
    }
    send({ action: 'set_location', parameters: { location: {
      latitude_e7: Math.round(lat * 10_000_000),
      longitude_e7: Math.round(lon * 10_000_000),
      altitude_mm: 5000,
      accuracy_mm: 5000,
    } } });
  };
  const applyBattery = () => {
    if (!Number.isInteger(battery) || battery < 0 || battery > 100) {
      setFormError('电量应为 0–100 的整数。');
      return;
    }
    send({ action: 'set_battery', parameters: { battery: {
      level_percent: battery,
      charging,
      temperature_deci_celsius: 250,
    } } });
  };
  const applyNetwork = () => {
    if (!Number.isInteger(latency) || latency < 0 || !Number.isFinite(lossPercent) || lossPercent < 0 || lossPercent > 100) {
      setFormError('延迟不能为负数，丢包率应为 0–100%。');
      return;
    }
    send({ action: 'set_network_condition', parameters: { condition: {
      latency_ms: latency,
      loss_basis_points: Math.round(lossPercent * 100),
      bandwidth_kbps: null,
    } } });
  };
  const injectSensor = () => {
    const values = sensorValues.split(',').map(value => Number(value.trim()));
    if (!values.length || values.some(value => !Number.isFinite(value))) {
      setFormError('传感器值必须是用逗号分隔的数字。');
      return;
    }
    send({ action: 'inject_sensor', parameters: { injection: {
      sensor,
      values_microunits: values,
      duration_ms: 0,
    } } });
  };
  const presentNfc = () => {
    const value = ndef.trim();
    if (!value || value.length % 2 !== 0 || !/^[0-9a-f]+$/i.test(value)) {
      setFormError('NDEF 必须是长度为偶数的十六进制字符串。');
      return;
    }
    send({ action: 'nfc_tag', parameters: { action: { command: 'present_type2', ndef_hex: value } } });
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
          <button type="button" disabled={!canUse('gnss')} onClick={applyLocation}>应用定位</button>
        </article>
        <article className="device-card">
          <h2><Battery size={17} />电池 <CapabilityState enabled={enabled('power')} adbReady={record.adb_ready} capability={hostCapability('power')} loading={capabilitiesLoading} /></h2>
          <CapabilitySummary capability={hostCapability('power')} loading={capabilitiesLoading} />
          <label><span>电量</span><input disabled={!canUse('power')} type="number" min={0} max={100} value={battery} onChange={event => setBattery(Number(event.target.value))} /></label>
          <label className="compact-check"><span>正在充电</span><input disabled={!canUse('power')} type="checkbox" checked={charging} onChange={event => setCharging(event.target.checked)} /></label>
          <button type="button" disabled={!canUse('power')} onClick={applyBattery}>应用电池</button>
        </article>
        <article className="device-card">
          <h2><Wifi size={17} />网络 <CapabilityState enabled={enabled('network')} adbReady={record.adb_ready} capability={hostCapability('network')} loading={capabilitiesLoading} /></h2>
          <CapabilitySummary capability={hostCapability('network')} loading={capabilitiesLoading} />
          <label><span>延迟 ms</span><input disabled={!canUse('network')} type="number" min={0} value={latency} onChange={event => setLatency(Number(event.target.value))} /></label>
          <label><span>丢包率 %</span><input disabled={!canUse('network')} type="number" min={0} max={100} step={0.1} value={lossPercent} onChange={event => setLossPercent(Number(event.target.value))} /></label>
          <button type="button" disabled={!canUse('network')} onClick={applyNetwork}>应用网络</button>
        </article>
        <article className="device-card">
          <h2><Activity size={17} />传感器 <CapabilityState enabled={enabled('sensors')} adbReady={record.adb_ready} capability={hostCapability('sensors')} loading={capabilitiesLoading} /></h2>
          <CapabilitySummary capability={hostCapability('sensors')} loading={capabilitiesLoading} />
          <select disabled={!canUse('sensors')} value={sensor} onChange={event => setSensor(event.target.value)}>
            {['accelerometer', 'gyroscope', 'magnetometer', 'light', 'proximity'].map(value => <option key={value}>{value}</option>)}
          </select>
          <label><span>微单位值</span><input disabled={!canUse('sensors')} value={sensorValues} onChange={event => setSensorValues(event.target.value)} /></label>
          <button type="button" disabled={!canUse('sensors')} onClick={injectSensor}>注入传感器</button>
        </article>
        <article className="device-card">
          <h2><Bluetooth size={17} />Bluetooth <CapabilityState enabled={enabled('bluetooth')} adbReady={record.adb_ready} capability={hostCapability('bluetooth')} loading={capabilitiesLoading} /></h2>
          <CapabilitySummary capability={hostCapability('bluetooth')} loading={capabilitiesLoading} />
          <label><span>Peer 名称</span><input disabled={!canUse('bluetooth')} value={peerName} onChange={event => setPeerName(event.target.value)} /></label>
          <div className="button-row">
            <button type="button" disabled={!canUse('bluetooth') || !peerName.trim()} onClick={() => {
              send({ action: 'bluetooth_peer', parameters: { action: { command: 'create_gatt_peer', peer_id: peerId, name: peerName.trim() } } });
              setPeerCreated(true);
            }}>创建</button>
            <button type="button" disabled={!canUse('bluetooth') || !peerCreated} onClick={() => send({ action: 'bluetooth_peer', parameters: { action: { command: 'set_advertising', peer_id: peerId, enabled: true } } })}>广播</button>
          </div>
        </article>
        <article className="device-card">
          <h2><Nfc size={17} />NFC <CapabilityState enabled={enabled('nfc')} adbReady={record.adb_ready} capability={hostCapability('nfc')} loading={capabilitiesLoading} /></h2>
          <CapabilitySummary capability={hostCapability('nfc')} loading={capabilitiesLoading} />
          <label><span>NDEF hex</span><input disabled={!canUse('nfc')} className="mono-input" value={ndef} onChange={event => setNdef(event.target.value)} /></label>
          <div className="button-row">
            <button type="button" disabled={!canUse('nfc')} onClick={presentNfc}>放置 Type 2</button>
            <button type="button" disabled={!canUse('nfc')} onClick={() => send({ action: 'nfc_tag', parameters: { action: { command: 'remove' } } })}>移除</button>
          </div>
        </article>
        <article className="device-card passive-device-card">
          <h2><Radio size={17} />UWB <CapabilityState enabled={enabled('uwb')} adbReady={record.adb_ready} capability={hostCapability('uwb')} loading={capabilitiesLoading} /></h2>
          <CapabilitySummary capability={hostCapability('uwb')} loading={capabilitiesLoading} />
          <p>FiRa 会话由实例启动配置管理；当前控制协议尚未提供运行时会话编辑。</p>
        </article>
        <article className="device-card passive-device-card">
          <h2><Signal size={17} />蜂窝网络 <CapabilityState enabled={enabled('modem')} adbReady={record.adb_ready} capability={hostCapability('modem')} loading={capabilitiesLoading} /></h2>
          <CapabilitySummary capability={hostCapability('modem')} loading={capabilitiesLoading} />
          <p>基础 AT、信号、运营商和注册状态随实例后端启动。</p>
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
    </section>
  );
}

function DiagnosticsPage({
  record,
  status,
  artifactHint,
  busy,
}: {
  record: InstanceRecord | null;
  status: string;
  artifactHint: string | null;
  busy: boolean;
}) {
  const [includeGuestLogs, setIncludeGuestLogs] = useState(true);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [copyState, setCopyState] = useState<'idle' | 'copied' | 'failed'>('idle');
  const observed = record?.status.observed;
  const canDelete = Boolean(record && ['defined', 'stopped', 'failed', 'blocked'].includes(observed ?? ''));
  return (
    <section className="simple-page diagnostics-page">
      <h1>诊断</h1>
      <article className="diagnostic-card">
        <h2><FileArchive size={17} />诊断包</h2>
        <p>{status}</p>
        <label className="option-row">
          <input type="checkbox" checked={includeGuestLogs} onChange={event => setIncludeGuestLogs(event.target.checked)} />
          <span>包含 Guest 日志</span>
        </label>
        <button type="button" disabled={busy || !record} onClick={() => post({ command: 'diagnostics', include_guest_logs: includeGuestLogs })}>
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
          </div>
        )}
      </article>
      <article className="diagnostic-card danger-zone">
        <h2><Trash2 size={17} />删除实例</h2>
        <p>{canDelete ? '删除实例、磁盘和运行记录；此操作不可撤销。' : '必须先关闭实例，才能执行删除。'}</p>
        <button type="button" disabled={busy || !canDelete} onClick={() => setConfirmDelete(true)}>删除当前实例</button>
      </article>
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
  display_maximized: boolean;
  android_focused: boolean;
  sidebar_visible: boolean;
}

const emptyLayout: LayoutState = {
  page: 'player',
  sidebar_collapsed: true,
  apk_path: '',
  display_maximized: false,
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
      if (message.type === 'layout') setLayout(message.payload as LayoutState);
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

function TopSurface() {
  const { snapshot, layout, busy } = useHostState();
  const sidebarTitle = layout.sidebar_visible ? '折叠侧栏' : '展开侧栏';
  return (
    <Tooltip.Provider>
      <header className={`titlebar surface-top${isMacOS ? ' macos-titlebar' : ''}`} onDoubleClick={() => post({ command: 'window', action: 'maximize' })}>
        <button
          type="button"
          className="sidebar-toggle"
          aria-label={sidebarTitle}
          title={sidebarTitle}
          onMouseDown={event => event.stopPropagation()}
          onDoubleClick={event => event.stopPropagation()}
          onClick={() => post({ command: 'toggle_sidebar' })}
        >
          {layout.sidebar_visible ? <PanelLeftClose size={19} /> : <PanelLeftOpen size={19} />}
        </button>
        <div
          className="drag-region"
          aria-label="拖动窗口"
          onMouseDown={event => { if (event.button === 0) post({ command: 'window', action: 'drag' }); }}
        />
        {layout.page === 'player' && (
          <PlayerTools
            record={snapshot.selected}
            busy={busy}
            apkPath={layout.apk_path}
            displayMaximized={layout.android_focused}
          />
        )}
        {!isMacOS && (
          <div className="window-controls">
            <button type="button" aria-label="最小化窗口" title="最小化" onClick={() => post({ command: 'window', action: 'minimize' })}><Minus size={16} /></button>
            <button type="button" aria-label="最大化或还原窗口" title="最大化或还原" onClick={() => post({ command: 'window', action: 'maximize' })}><Square size={13} /></button>
            <button type="button" className="close" aria-label="关闭 HD，实例继续运行" title="关闭 HD，实例继续运行" onClick={() => post({ command: 'window', action: 'close' })}><X size={17} /></button>
          </div>
        )}
      </header>
    </Tooltip.Provider>
  );
}

function SidebarSurface() {
  const { snapshot, layout, busy, notice } = useHostState();
  const [query, setQuery] = useState('');
  const [creating, setCreating] = useState(false);
  const [newName, setNewName] = useState('Android');
  const [powerMenu, setPowerMenu] = useState<{ x: number; y: number; instanceId: string } | null>(null);
  const filtered = snapshot.summaries.filter(item => item.name.toLowerCase().includes(query.toLowerCase()));
  const selectedId = snapshot.selected?.spec.id;
  const observed = snapshot.selected?.status.observed ?? '';
  const transitional = transitionStates.has(observed);
  const menuTargetReady = Boolean(powerMenu && selectedId === powerMenu.instanceId);
  const canStart = menuTargetReady && !busy && !transitional && ['defined', 'stopped', 'failed', 'blocked'].includes(observed);
  const canPause = menuTargetReady && !busy && observed === 'ready';
  const canResume = menuTargetReady && !busy && observed === 'paused';
  const safePowerControl = Boolean(snapshot.selected?.adb_ready);
  const canRestart = menuTargetReady && safePowerControl && !busy && ['ready', 'paused'].includes(observed);
  const canStop = menuTargetReady && safePowerControl && !busy && ['ready', 'paused'].includes(observed);
  const sidebarNotice = ['ready', 'paused'].includes(observed) && !safePowerControl
    ? 'ADB 控制未就绪；已禁用关机与重启以保护实例数据'
    : notice;

  useEffect(() => {
    const closeOnBlur = () => post({ command: 'close_sidebar' });
    window.addEventListener('blur', closeOnBlur);
    return () => window.removeEventListener('blur', closeOnBlur);
  }, []);

  const cancelCreate = () => {
    setCreating(false);
    setNewName('Android');
  };
  const submitCreate = () => {
    const name = newName.trim();
    if (!name || busy) return;
    post({ command: 'create', name });
    cancelCreate();
  };
  const navigate = (page: Page) => post({ command: 'page', page });
  const operation = (kind: string) => {
    setPowerMenu(null);
    post({ command: 'operation', kind });
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
            setCreating(true);
          }
        }}>
          {creating ? <X size={18} /> : <Plus size={18} />}
          <span>{creating ? '取消新建' : '新建实例'}</span>
        </button>
        {creating && (
          <form className="create-row" onSubmit={event => { event.preventDefault(); submitCreate(); }}>
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
      <NoticeBanner message={sidebarNotice} busy={busy} />
      <nav className="sidebar-nav" aria-label="主要页面">
        <button type="button" className={layout.page === 'player' ? 'active' : ''} onClick={() => navigate('player')}><AppWindow size={18} /><span>Player</span></button>
        <button type="button" className={layout.page === 'settings' ? 'active' : ''} onClick={() => navigate('settings')}><Settings size={18} /><span>设置</span></button>
        <button type="button" className={layout.page === 'devices' ? 'active' : ''} onClick={() => navigate('devices')}><Boxes size={18} /><span>设备</span></button>
        <button type="button" className={layout.page === 'diagnostics' ? 'active' : ''} onClick={() => navigate('diagnostics')}><Gauge size={18} /><span>诊断</span></button>
      </nav>
      {powerMenu && (
        <div className="sidebar-context-menu" role="menu" style={{ left: powerMenu.x, top: powerMenu.y }} onContextMenu={event => event.preventDefault()}>
          <button type="button" role="menuitem" disabled={!canStart} onClick={() => operation('start')}><Play size={16} />启动</button>
          <button type="button" role="menuitem" disabled={observed === 'paused' ? !canResume : !canPause} onClick={() => operation(observed === 'paused' ? 'resume' : 'pause')}>
            <CirclePause size={16} />{observed === 'paused' ? '恢复' : '暂停'}
          </button>
          <div className="context-menu-separator" />
          <button type="button" role="menuitem" disabled={!canRestart} onClick={() => operation('restart')}><RefreshCw size={16} />重启</button>
          <button type="button" role="menuitem" className="danger" disabled={!canStop} onClick={() => operation('stop')}><Power size={16} />关机</button>
        </div>
      )}
    </aside>
  );
}

function ContentSurface() {
  const { snapshot, layout, busy, notice } = useHostState();
  const [apkPath, setApkPath] = useState(layout.apk_path);
  useEffect(() => setApkPath(layout.apk_path), [layout.apk_path]);
  const updateApkPath = (value: string) => {
    setApkPath(value);
    post({ command: 'set_apk_path', path: value });
  };
  return (
    <main className="main-panel surface-content">
      {layout.page === 'settings' && (
        <SettingsPage
          record={snapshot.selected}
          busy={busy}
          apkPath={apkPath}
          setApkPath={updateApkPath}
          onChooseApk={() => post({ command: 'choose_apk' })}
          onInstallApk={() => post({ command: 'install_apk', path: apkPath })}
          onSave={(spec, restart) => post({ command: 'save_spec', spec, restart })}
          deviceCapabilities={snapshot.device_capabilities}
          capabilitiesLoading={snapshot.device_capabilities_loading}
        />
      )}
      {layout.page === 'devices' && (
        <DevicesPage
          record={snapshot.selected}
          busy={busy}
          deviceCapabilities={snapshot.device_capabilities}
          capabilitiesLoading={snapshot.device_capabilities_loading}
        />
      )}
      {layout.page === 'diagnostics' && (
        <DiagnosticsPage
          record={snapshot.selected}
          status={snapshot.status}
          artifactHint={snapshot.artifact_hint}
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

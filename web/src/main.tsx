import React, { useEffect, useMemo, useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import * as Tooltip from '@radix-ui/react-tooltip';
import {
  Activity, AppWindow, ArrowLeft, Battery, Bluetooth, Boxes, Camera, ChevronDown,
  CirclePause, Expand, FileArchive, Gauge, HardDrive, Home, MapPin, Minus,
  MonitorCog, MoreHorizontal, Nfc, PackagePlus, Play, Plus, Power, RefreshCw,
  PanelLeftClose, PanelLeftOpen, RotateCw, Search, Settings, SlidersHorizontal, Square, SquareStack, Trash2,
  Volume1, Volume2, Wifi, X,
} from 'lucide-react';
import type { LucideIcon } from 'lucide-react';
import { post, type HostMessage } from './bridge';
import type { InstanceRecord, InstanceSpec, InstanceSummary, Snapshot } from './types';
import './style.css';

type Page = 'player' | 'settings' | 'devices' | 'diagnostics';
type IconType = LucideIcon;

const emptySnapshot: Snapshot = { summaries: [], selected: null, status: '正在连接 HD Host…', artifact_hint: null };

function ToolButton({ label, icon: Icon, onClick, disabled = false }: {
  label: string; icon: IconType; onClick: () => void; disabled?: boolean;
}) {
  return (
    <Tooltip.Root delayDuration={350}>
      <Tooltip.Trigger asChild>
        <button className="icon-button" aria-label={label} disabled={disabled} onClick={onClick}>
          <Icon size={19} strokeWidth={1.8} />
        </button>
      </Tooltip.Trigger>
      <Tooltip.Portal><Tooltip.Content className="tooltip" sideOffset={8}>{label}</Tooltip.Content></Tooltip.Portal>
    </Tooltip.Root>
  );
}

function stateLabel(state?: string) {
  const labels: Record<string, string> = {
    defined: '未启动', preparing: '准备中', starting_worker: '启动 Worker', launching_guest: '启动 Guest',
    negotiating_display: '连接显示', guest_booting: 'Android 启动中', adb_connecting: '连接 ADB',
    ready: '运行中', pausing: '暂停中', paused: '已暂停', resuming: '恢复中', recovering: '恢复中',
    stopping: '停止中', stopped: '已停止', blocked: '已阻止', failed: '失败', deleting: '删除中', deleted: '已删除',
  };
  return labels[state ?? ''] ?? state ?? '未知';
}

function InstanceRow({ item, selected, onClick, onContextMenu }: {
  item: InstanceSummary;
  selected: boolean;
  onClick: () => void;
  onContextMenu: (event: React.MouseEvent<HTMLButtonElement>) => void;
}) {
  const active = ['ready', 'running', 'paused'].includes(item.status.observed);
  return (
    <button className={`instance-row ${selected ? 'selected' : ''}`} onClick={onClick} onContextMenu={onContextMenu}>
      <span className={`state-dot ${active ? 'active' : ''}`} />
      <span className="instance-copy">
        <span className="instance-name">{item.name}</span>
        <span className="instance-meta">{stateLabel(item.status.observed)} · 帧 {item.frame_generation}</span>
      </span>
      <MoreHorizontal size={17} strokeWidth={1.8} />
    </button>
  );
}

function AndroidViewport({ record }: { record: InstanceRecord | null }) {
  const host = useRef<HTMLDivElement>(null);
  const surface = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const container = host.current;
    const target = surface.current;
    if (!container || !target) return;
    let revision = 0;
    let frame = 0;
    let lastRect: DOMRect | null = null;
    const update = () => {
      const c = container.getBoundingClientRect();
      if (document.visibilityState !== 'visible' || c.width < 64 || c.height < 64) return;
      const orientation = record?.spec.display.orientation ?? 'portrait';
      const portrait = orientation === 'portrait' || orientation === 'reverse_portrait';
      const nativeW = record?.spec.display.width ?? 1080;
      const nativeH = record?.spec.display.height ?? 1920;
      const guestW = portrait ? Math.min(nativeW, nativeH) : Math.max(nativeW, nativeH);
      const guestH = portrait ? Math.max(nativeW, nativeH) : Math.min(nativeW, nativeH);
      const scale = Math.min(c.width / guestW, c.height / guestH);
      const width = Math.max(1, guestW * scale);
      const height = Math.max(1, guestH * scale);
      target.style.width = `${width}px`;
      target.style.height = `${height}px`;
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => {
        const rect = target.getBoundingClientRect();
        if (document.visibilityState !== 'visible' || rect.width < 64 || rect.height < 64) return;
        lastRect = rect;
        post({ command: 'viewport', x: rect.x, y: rect.y, width: rect.width, height: rect.height,
          scale_factor: window.devicePixelRatio, visible: true, revision: ++revision });
      });
    };
    const observer = new ResizeObserver(update);
    observer.observe(container);
    window.addEventListener('resize', update);
    document.addEventListener('visibilitychange', update);
    update();
    return () => {
      observer.disconnect();
      window.removeEventListener('resize', update);
      document.removeEventListener('visibilitychange', update);
      cancelAnimationFrame(frame);
      const rect = lastRect;
      post({ command: 'viewport', x: rect?.x ?? 0, y: rect?.y ?? 0,
        width: rect?.width ?? 1, height: rect?.height ?? 1,
        scale_factor: window.devicePixelRatio, visible: false, revision: ++revision });
    };
  }, [record?.spec.display.width, record?.spec.display.height, record?.spec.display.orientation]);

  return <div ref={host} className="viewport-host" onMouseDown={() => post({ command: 'focus_display' })}>
    <div ref={surface} className="android-surface" />
  </div>;
}

function PlayerTools({ record, busy, apkPath }: { record: InstanceRecord | null; busy: boolean; apkPath: string }) {
  const action = (command: string, extra: Record<string, unknown> = {}) => post({ command, ...extra });
  const orientation = record?.spec.display.orientation ?? 'portrait';
  const nextOrientation = orientation.includes('landscape') ? 'portrait' : 'landscape';
  return <div className="tool-group">
    <ToolButton label="音量减" icon={Volume1} onClick={() => action('key', { key: 'volume_down' })} disabled={!record || busy} />
    <ToolButton label="音量加" icon={Volume2} onClick={() => action('key', { key: 'volume_up' })} disabled={!record || busy} />
    <ToolButton label="旋转" icon={RotateCw} onClick={() => action('rotate', { orientation: nextOrientation })} disabled={!record || busy} />
    <ToolButton label="截图" icon={Camera} onClick={() => action('screenshot')} disabled={!record || busy} />
    <ToolButton label="安装 APK" icon={PackagePlus} onClick={() => action('install_apk', { path: apkPath })} disabled={!record || busy || !apkPath} />
    <ToolButton label="全屏" icon={Expand} onClick={() => action('window', { action: 'fullscreen' })} />
    <span className="tool-divider" />
    <ToolButton label="最近任务" icon={SquareStack} onClick={() => action('key', { key: 'recent' })} disabled={!record || busy} />
    <ToolButton label="主页" icon={Home} onClick={() => action('key', { key: 'home' })} disabled={!record || busy} />
    <ToolButton label="返回" icon={ArrowLeft} onClick={() => action('key', { key: 'back' })} disabled={!record || busy} />
  </div>;
}

function Player({ record }: { record: InstanceRecord | null }) {
  return <section className="player-page"><AndroidViewport record={record} /></section>;
}

const settingSections = [
  ['general', '常规', Settings], ['performance', '性能', Gauge], ['display', '显示', MonitorCog],
  ['adb', 'Android / ADB', AppWindow], ['artifacts', '制品与启动', HardDrive],
  ['devices', '设备模拟', Boxes], ['advanced', '高级', SlidersHorizontal],
] as const;

function NumberField({ label, value, onChange, min, max, suffix }: {
  label: string; value: number; onChange: (n: number) => void; min: number; max: number; suffix?: string;
}) {
  return <label className="setting-row"><span>{label}</span><span className="field-with-suffix">
    <input type="number" min={min} max={max} value={value} onChange={e => onChange(Number(e.target.value))} />
    {suffix && <small>{suffix}</small>}
  </span></label>;
}

function SettingsPage({ record, onSave, apkPath, setApkPath }: {
  record: InstanceRecord | null; onSave: (spec: InstanceSpec) => void; apkPath: string; setApkPath: (v: string) => void;
}) {
  const [section, setSection] = useState('general');
  const [draft, setDraft] = useState<InstanceSpec | null>(record ? structuredClone(record.spec) : null);
  useEffect(() => setDraft(record ? structuredClone(record.spec) : null), [record?.spec.id, record?.status.revision]);
  if (!draft) return <div className="empty-page">请先选择一个实例</div>;
  const mutate = (fn: (next: InstanceSpec) => void) => setDraft(current => {
    if (!current) return current;
    const next = structuredClone(current); fn(next); return next;
  });
  return <section className="settings-page">
    <header className="content-header"><div className="content-title">设置</div>
      <button className="primary-button" onClick={() => onSave(draft)}>保存更改</button>
    </header>
    <div className="settings-body">
      <nav className="settings-nav">
        {settingSections.map(([id, label, Icon]) => <button key={id} className={section === id ? 'active' : ''} onClick={() => setSection(id)}>
          <Icon size={18} strokeWidth={1.8} /><span>{label}</span>
        </button>)}
      </nav>
      <div className="settings-content">
        {section === 'general' && <><h2>常规</h2>
          <label className="setting-row"><span>实例名称</span><input value={draft.name} onChange={e => mutate(n => n.name = e.target.value)} /></label>
          <label className="setting-row"><span>异常后自动重启</span><input type="checkbox" checked={draft.restart_policy === 'on_failure'} onChange={e => mutate(n => n.restart_policy = e.target.checked ? 'on_failure' : 'never')} /></label>
        </>}
        {section === 'performance' && <><h2>性能</h2>
          <NumberField label="处理器" value={draft.cpu_count} min={1} max={256} suffix="核" onChange={v => mutate(n => n.cpu_count = v)} />
          <NumberField label="内存" value={draft.memory_mib} min={2048} max={1048576} suffix="MiB" onChange={v => mutate(n => n.memory_mib = v)} />
        </>}
        {section === 'display' && <><h2>显示</h2>
          <NumberField label="宽度" value={draft.display.width} min={320} max={8192} suffix="px" onChange={v => mutate(n => n.display.width = v)} />
          <NumberField label="高度" value={draft.display.height} min={320} max={8192} suffix="px" onChange={v => mutate(n => n.display.height = v)} />
          <NumberField label="DPI" value={draft.display.dpi} min={72} max={960} onChange={v => mutate(n => n.display.dpi = v)} />
          <label className="setting-row"><span>刷新率</span><select value={draft.display.refresh_rate_hz} onChange={e => mutate(n => n.display.refresh_rate_hz = Number(e.target.value))}>
            {[30, 60, 90, 120].map(v => <option key={v} value={v}>{v} Hz</option>)}
          </select></label>
        </>}
        {section === 'adb' && <><h2>Android / ADB</h2>
          <label className="setting-row"><span>ADB</span><select value={draft.adb.mode} onChange={e => mutate(n => n.adb.mode = e.target.value as 'disabled' | 'loopback')}>
            <option value="loopback">本机回环</option><option value="disabled">禁用</option>
          </select></label>
          <label className="setting-row"><span>ADB 可执行文件</span><input value={draft.adb.executable ?? ''} onChange={e => mutate(n => n.adb.executable = e.target.value || null)} /></label>
          <label className="setting-row"><span>APK 文件</span><input value={apkPath} placeholder="C:\\path\\app.apk" onChange={e => setApkPath(e.target.value)} /></label>
        </>}
        {section === 'artifacts' && <><h2>制品与启动</h2>
          {!draft.artifacts ? <button className="secondary-button" onClick={() => mutate(n => n.artifacts = {
            store_root: 'D:\\hd-v2-artifact-store',
            guest_bundle_digest: '22281c84556c6e865e4f94498968efc2f651ef4c37bd3e871d930414609b2986',
            host_bundle_digest: '5187de8d05cf29fdbed127c72ab0a96b50fa37f800137079488237208ae1aa7e',
          })}>使用本机开发制品</button> : <>
            <label className="setting-row"><span>制品仓库</span><input value={draft.artifacts.store_root} onChange={e => mutate(n => { if (n.artifacts) n.artifacts.store_root = e.target.value; })} /></label>
            <label className="setting-row"><span>Guest digest</span><input className="mono-input" value={draft.artifacts.guest_bundle_digest} onChange={e => mutate(n => { if (n.artifacts) n.artifacts.guest_bundle_digest = e.target.value; })} /></label>
            <label className="setting-row"><span>Host digest</span><input className="mono-input" value={draft.artifacts.host_bundle_digest} onChange={e => mutate(n => { if (n.artifacts) n.artifacts.host_bundle_digest = e.target.value; })} /></label>
            <button className="secondary-button danger" onClick={() => mutate(n => n.artifacts = null)}>清除制品选择</button>
          </>}
        </>}
        {section === 'devices' && <><h2>设备模拟</h2>{Object.entries(draft.devices).map(([key, enabled]) =>
          <label className="setting-row" key={key}><span>{key}</span><input type="checkbox" checked={enabled} onChange={e => mutate(n => n.devices[key] = e.target.checked)} /></label>)}</>}
        {section === 'advanced' && <><h2>高级</h2>
          <NumberField label="Kernel 日志级别" value={draft.boot.kernel_log_level} min={0} max={7} onChange={v => mutate(n => n.boot.kernel_log_level = v)} />
          <NumberField label="Panic 超时" value={draft.boot.panic_timeout_seconds} min={0} max={300} suffix="秒" onChange={v => mutate(n => n.boot.panic_timeout_seconds = v)} />
          <label className="setting-row"><span>启动动画</span><input type="checkbox" checked={draft.boot.boot_animation} onChange={e => mutate(n => n.boot.boot_animation = e.target.checked)} /></label>
        </>}
      </div>
    </div>
  </section>;
}

function DevicesPage({ disabled }: { disabled: boolean }) {
  const [latitude, setLatitude] = useState(374219999);
  const [longitude, setLongitude] = useState(-1220840577);
  const [battery, setBattery] = useState(100);
  const [charging, setCharging] = useState(true);
  const [latency, setLatency] = useState(0);
  const [loss, setLoss] = useState(0);
  const [sensor, setSensor] = useState('accelerometer');
  const [sensorValues, setSensorValues] = useState('0,0,9806650');
  const [peerId] = useState(() => crypto.randomUUID());
  const [peerName, setPeerName] = useState('HD GATT peer');
  const [ndef, setNdef] = useState('D1010B5402656E48656C6C6F204844');
  const send = (action: Record<string, unknown>) => post({ command: 'action', action });
  return <section className="simple-page devices-page"><h1>设备模拟</h1>
    <div className="device-grid">
      <div className="device-card"><h2><MapPin size={18} />定位</h2>
        <label>纬度 E7<input type="number" value={latitude} onChange={e => setLatitude(Number(e.target.value))} /></label>
        <label>经度 E7<input type="number" value={longitude} onChange={e => setLongitude(Number(e.target.value))} /></label>
        <button disabled={disabled} onClick={() => send({ action: 'set_location', parameters: { location: { latitude_e7: latitude, longitude_e7: longitude, altitude_mm: 5000, accuracy_mm: 5000 } } })}>应用定位</button>
      </div>
      <div className="device-card"><h2><Battery size={18} />电池</h2>
        <label>电量<input type="number" min="0" max="100" value={battery} onChange={e => setBattery(Number(e.target.value))} /></label>
        <label className="compact-check">正在充电<input type="checkbox" checked={charging} onChange={e => setCharging(e.target.checked)} /></label>
        <button disabled={disabled} onClick={() => send({ action: 'set_battery', parameters: { battery: { level_percent: battery, charging, temperature_deci_celsius: 250 } } })}>应用电池</button>
      </div>
      <div className="device-card"><h2><Wifi size={18} />网络</h2>
        <label>延迟 ms<input type="number" min="0" value={latency} onChange={e => setLatency(Number(e.target.value))} /></label>
        <label>丢包基点<input type="number" min="0" max="10000" value={loss} onChange={e => setLoss(Number(e.target.value))} /></label>
        <button disabled={disabled} onClick={() => send({ action: 'set_network_condition', parameters: { condition: { latency_ms: latency, loss_basis_points: loss, bandwidth_kbps: null } } })}>应用网络</button>
      </div>
      <div className="device-card"><h2><Activity size={18} />传感器</h2>
        <select value={sensor} onChange={e => setSensor(e.target.value)}><option>accelerometer</option><option>gyroscope</option><option>magnetometer</option><option>light</option><option>proximity</option></select>
        <label>微单位值<input value={sensorValues} onChange={e => setSensorValues(e.target.value)} /></label>
        <button disabled={disabled} onClick={() => send({ action: 'inject_sensor', parameters: { injection: { sensor, values_microunits: sensorValues.split(',').map(Number), duration_ms: 0 } } })}>注入传感器</button>
      </div>
      <div className="device-card"><h2><Bluetooth size={18} />Bluetooth</h2>
        <label>Peer 名称<input value={peerName} onChange={e => setPeerName(e.target.value)} /></label>
        <div className="button-row"><button disabled={disabled} onClick={() => send({ action: 'bluetooth_peer', parameters: { action: { command: 'create_gatt_peer', peer_id: peerId, name: peerName } } })}>创建</button>
          <button disabled={disabled} onClick={() => send({ action: 'bluetooth_peer', parameters: { action: { command: 'set_advertising', peer_id: peerId, enabled: true } } })}>广播</button></div>
      </div>
      <div className="device-card"><h2><Nfc size={18} />NFC</h2>
        <label>NDEF hex<input className="mono-input" value={ndef} onChange={e => setNdef(e.target.value)} /></label>
        <div className="button-row"><button disabled={disabled} onClick={() => send({ action: 'nfc_tag', parameters: { action: { command: 'present_type2', ndef_hex: ndef } } })}>Type 2</button>
          <button disabled={disabled} onClick={() => send({ action: 'nfc_tag', parameters: { action: { command: 'remove' } } })}>移除</button></div>
      </div>
    </div>
  </section>;
}

function DiagnosticsPage({ status, artifactHint, disabled }: { status: string; artifactHint: string | null; disabled: boolean }) {
  return <section className="simple-page diagnostics-page"><h1>诊断</h1>
    <div className="diagnostic-card"><h2><FileArchive size={19} />诊断包</h2><p>{status}</p>
      <div className="button-row"><button disabled={disabled} onClick={() => post({ command: 'diagnostics', include_guest_logs: false })}>生成诊断包</button>
        <button disabled={disabled} onClick={() => post({ command: 'diagnostics', include_guest_logs: true })}>包含 Guest 日志</button></div>
    </div>
    {artifactHint && <code>{artifactHint}</code>}
    <div className="diagnostic-card danger-zone"><h2><Trash2 size={19} />删除实例</h2><p>实例必须先停止；删除操作不可撤销。</p>
      <button disabled={disabled} onClick={() => post({ command: 'operation', kind: 'delete' })}>删除当前实例</button>
    </div>
  </section>;
}

interface LayoutState {
  page: Page;
  sidebar_collapsed: boolean;
  apk_path: string;
}

const emptyLayout: LayoutState = { page: 'player', sidebar_collapsed: false, apk_path: '' };

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
      if (message.type === 'notice') { setNotice(String(message.payload ?? '')); setBusy(false); }
    };
    const surface = new URLSearchParams(window.location.search).get('surface') ?? 'content';
    post({ command: 'ready', surface });
    return () => { delete window.__hdReceive; };
  }, []);

  return { snapshot, layout, busy, notice };
}

function TopSurface() {
  const { snapshot, layout, busy } = useHostState();
  return <Tooltip.Provider><header className="titlebar surface-top" onDoubleClick={() => post({ command: 'window', action: 'maximize' })}>
    <button
      className="sidebar-toggle"
      aria-label={layout.sidebar_collapsed ? '展开侧栏' : '折叠侧栏'}
      title={layout.sidebar_collapsed ? '展开侧栏' : '折叠侧栏'}
      onMouseDown={event => event.stopPropagation()}
      onDoubleClick={event => event.stopPropagation()}
      onClick={() => post({ command: 'toggle_sidebar' })}
    >
      {layout.sidebar_collapsed ? <PanelLeftOpen size={19} /> : <PanelLeftClose size={19} />}
    </button>
    <div className="drag-region" aria-label="拖动窗口"
      onMouseDown={e => { if (e.button === 0) post({ command: 'window', action: 'drag' }); }} />
    {layout.page === 'player' && <PlayerTools record={snapshot.selected} busy={busy} apkPath={layout.apk_path} />}
    <div className="window-controls">
      <button onClick={() => post({ command: 'window', action: 'minimize' })}><Minus size={16} /></button>
      <button onClick={() => post({ command: 'window', action: 'maximize' })}><Square size={13} /></button>
      <button className="close" onClick={() => post({ command: 'window', action: 'close' })}><X size={17} /></button>
    </div>
  </header></Tooltip.Provider>;
}

function SidebarSurface() {
  const { snapshot, layout, busy } = useHostState();
  const [query, setQuery] = useState('');
  const [newName, setNewName] = useState('Android');
  const [creating, setCreating] = useState(false);
  const [powerMenu, setPowerMenu] = useState<{ x: number; y: number } | null>(null);
  const selectedId = snapshot.selected?.spec.id;
  const filtered = useMemo(() => snapshot.summaries.filter(i => i.name.toLowerCase().includes(query.toLowerCase())), [snapshot.summaries, query]);
  const operation = (kind: string) => {
    setPowerMenu(null);
    post({ command: 'operation', kind });
  };
  const navigate = (page: Page) => post({ command: 'page', page });
  const openPowerMenu = (event: React.MouseEvent<HTMLElement>) => {
    if ((event.target as HTMLElement).closest('input')) return;
    event.preventDefault();
    event.stopPropagation();
    const menuWidth = 184;
    const menuHeight = 178;
    setPowerMenu({
      x: Math.max(8, Math.min(event.clientX, window.innerWidth - menuWidth - 8)),
      y: Math.max(8, Math.min(event.clientY, window.innerHeight - menuHeight - 8)),
    });
  };
  useEffect(() => {
    if (!powerMenu) return;
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
  const paused = snapshot.selected?.status.observed === 'paused';
  const powerDisabled = busy || !snapshot.selected;
  return <aside className="sidebar surface-sidebar" onContextMenu={openPowerMenu}>
    <div className="sidebar-top">
      <button className="new-button" onClick={() => setCreating(v => !v)}><Plus size={18} /><span>新建实例</span></button>
      {creating && <div className="create-row"><input autoFocus value={newName} onChange={e => setNewName(e.target.value)} onKeyDown={e => {
        if (e.key === 'Enter' && newName.trim()) { post({ command: 'create', name: newName.trim() }); setCreating(false); }
      }} /><button onClick={() => { post({ command: 'create', name: newName.trim() || 'Android' }); setCreating(false); }}><Play size={16} /></button></div>}
      <div className="search"><Search size={16} /><input value={query} onChange={e => setQuery(e.target.value)} placeholder="搜索实例" /></div>
    </div>
    <div className="sidebar-label"><span>实例</span><ChevronDown size={15} /></div>
    <div className="instance-list">{filtered.map(item => <InstanceRow
      key={item.id}
      item={item}
      selected={selectedId === item.id}
      onClick={() => {
        post({ command: 'select', instance_id: item.id }); navigate('player');
      }}
      onContextMenu={event => {
        if (selectedId !== item.id) {
          post({ command: 'select', instance_id: item.id });
          navigate('player');
        }
        openPowerMenu(event);
      }}
    />)}</div>
    <nav className="sidebar-nav">
      <button className={layout.page === 'player' ? 'active' : ''} onClick={() => navigate('player')}><AppWindow size={18} /><span>Player</span></button>
      <button className={layout.page === 'settings' ? 'active' : ''} onClick={() => navigate('settings')}><Settings size={18} /><span>设置</span></button>
      <button className={layout.page === 'devices' ? 'active' : ''} onClick={() => navigate('devices')}><Boxes size={18} /><span>设备</span></button>
      <button className={layout.page === 'diagnostics' ? 'active' : ''} onClick={() => navigate('diagnostics')}><Gauge size={18} /><span>诊断</span></button>
    </nav>
    {powerMenu && <div
      className="sidebar-context-menu"
      role="menu"
      style={{ left: powerMenu.x, top: powerMenu.y }}
      onContextMenu={event => event.preventDefault()}
    >
      <button role="menuitem" disabled={powerDisabled} onClick={() => operation('start')}><Play size={16} />启动</button>
      <button role="menuitem" disabled={powerDisabled} onClick={() => operation(paused ? 'resume' : 'pause')}>
        <CirclePause size={16} />{paused ? '恢复' : '暂停'}
      </button>
      <div className="context-menu-separator" />
      <button role="menuitem" disabled={powerDisabled} onClick={() => operation('restart')}><RefreshCw size={16} />重启</button>
      <button role="menuitem" className="danger" disabled={powerDisabled} onClick={() => operation('stop')}><Power size={16} />关机</button>
    </div>}
  </aside>;
}

function ContentSurface() {
  const { snapshot, layout, busy } = useHostState();
  const [apkPath, setApkPath] = useState(layout.apk_path);
  useEffect(() => setApkPath(layout.apk_path), [layout.apk_path]);
  const updateApkPath = (value: string) => {
    setApkPath(value);
    post({ command: 'set_apk_path', path: value });
  };
  return <main className="main-panel surface-content">
    {layout.page === 'settings' && <SettingsPage record={snapshot.selected} apkPath={apkPath} setApkPath={updateApkPath} onSave={spec => post({ command: 'save_spec', spec })} />}
    {layout.page === 'devices' && <DevicesPage disabled={busy || !snapshot.selected} />}
    {layout.page === 'diagnostics' && <DiagnosticsPage status={snapshot.status} artifactHint={snapshot.artifact_hint} disabled={busy || !snapshot.selected} />}
  </main>;
}

function SurfaceApp() {
  const surface = new URLSearchParams(window.location.search).get('surface');
  if (surface === 'top') return <TopSurface />;
  if (surface === 'sidebar') return <SidebarSurface />;
  return <ContentSurface />;
}

createRoot(document.getElementById('root')!).render(<React.StrictMode><SurfaceApp /></React.StrictMode>);

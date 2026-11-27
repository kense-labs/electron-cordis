// TODO: Updating this file also required updating the module-keys file

// Browser side modules, please sort alphabetically.
export const browserModuleList: ElectronInternal.ModuleEntry[] = [
  { name: 'app', loader: () => require('./app') },
  { name: 'autoUpdater', loader: () => require('./auto-updater') },
  { name: 'BaseWindow', loader: () => require('./base-window') },
  ...(BUILDFLAG(CORDIS_HAS_BROWSER_VIEW) ? [
    { name: 'BrowserView', loader: () => require('./browser-view') }
  ] : []),
  { name: 'BrowserWindow', loader: () => require('./browser-window') },
  ...(BUILDFLAG(CORDIS_KEEP_LEGACY_CLIPBOARD) ? [
    { name: 'clipboard', loader: () => require('./clipboard') }
  ] : []),
  ...(BUILDFLAG(CORDIS_KEEP_LEGACY_CLIPBOARD) ? [
    { name: 'ClipboardItem', loader: () => require('./clipboard-item') }
  ] : []),
  ...(BUILDFLAG(CORDIS_HAS_CONTENT_TRACING) ? [
    { name: 'contentTracing', loader: () => require('./content-tracing') }
  ] : []),
  { name: 'crashReporter', loader: () => process._linkedBinding('electron_browser_crash_reporter') },
  { name: 'desktopCapturer', loader: () => require('./desktop-capturer') },
  { name: 'dialog', loader: () => require('./dialog') },
  { name: 'globalShortcut', loader: () => require('./global-shortcut') },
  { name: 'ipcMain', loader: () => require('./ipc-main') },
  { name: 'ImageView', loader: () => process._linkedBinding('electron_browser_image_view').ImageView },
  ...(BUILDFLAG(CORDIS_HAS_IN_APP_PURCHASE) ? [
    { name: 'inAppPurchase', loader: () => process._linkedBinding('electron_browser_in_app_purchase').inAppPurchase }
  ] : []),
  { name: 'Menu', loader: () => require('./menu') },
  { name: 'MenuItem', loader: () => require('./menu-item') },
  {
    name: 'MessageChannelMain',
    loader: () => process._linkedBinding('electron_browser_message_port').MessageChannelMain
  },
  { name: 'nativeTheme', loader: () => require('./native-theme') },
  ...(BUILDFLAG(CORDIS_HAS_ELECTRON_NET) ? [
    { name: 'net', loader: () => require('./net') }
  ] : []),
  ...(BUILDFLAG(CORDIS_HAS_NET_LOG) ? [
    { name: 'netLog', loader: () => require('./net-log') }
  ] : []),
  { name: 'Notification', loader: () => process._linkedBinding('electron_browser_notification').Notification },
  { name: 'powerMonitor', loader: () => require('./power-monitor') },
  { name: 'powerSaveBlocker', loader: () => require('./power-save-blocker') },
  ...(BUILDFLAG(CORDIS_HAS_PUSH_NOTIFICATIONS) ? [
    { name: 'pushNotifications', loader: () => require('./push-notifications') }
  ] : []),
  { name: 'protocol', loader: () => require('./protocol') },
  ...(BUILDFLAG(CORDIS_KEEP_LEGACY_SAFE_STORAGE) ? [
    { name: 'safeStorage', loader: () => require('./safe-storage') }
  ] : []),
  { name: 'screen', loader: () => require('./screen') },
  ...(BUILDFLAG(CORDIS_HAS_SERVICE_WORKER_MAIN) ? [
    { name: 'ServiceWorkerMain', loader: () => require('./service-worker-main') }
  ] : []),
  { name: 'session', loader: () => require('./session') },
  ...(BUILDFLAG(CORDIS_HAS_SHARED_TEXTURE) ? [
    { name: 'sharedTexture', loader: () => require('./shared-texture') }
  ] : []),
  ...(BUILDFLAG(CORDIS_HAS_SHARE_MENU) ? [
    { name: 'ShareMenu', loader: () => require('./share-menu') }
  ] : []),
  { name: 'systemPreferences', loader: () => require('./system-preferences') },
  ...(BUILDFLAG(CORDIS_HAS_TOUCH_BAR) ? [
    { name: 'TouchBar', loader: () => require('./touch-bar') }
  ] : []),
  { name: 'Tray', loader: () => require('./tray') },
  { name: 'utilityProcess', loader: () => require('./utility-process') },
  { name: 'View', loader: () => process._linkedBinding('electron_browser_view').View },
  { name: 'webContents', loader: () => require('./web-contents') },
  {
    name: 'WebContentsView',
    loader: () => process._linkedBinding('electron_browser_web_contents_view').WebContentsView
  },
  { name: 'webFrameMain', loader: () => require('./web-frame-main') }
];

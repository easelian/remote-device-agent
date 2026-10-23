import path from 'node:path';

export function validateInheritedEnv(value: unknown): string[] {
  if (
    !Array.isArray(value) ||
    value.length > 64 ||
    value.some((name) => typeof name !== 'string' || !/^[A-Za-z_][A-Za-z0-9_]{0,127}$/.test(name))
  ) {
    throw new Error('inherit_env must contain at most 64 environment variable names');
  }
  return [...new Set(value as string[])];
}

export function processEnvironment({
  parent = process.env,
  platform = process.platform,
  inherit = [],
  session = true,
}: {
  parent?: NodeJS.ProcessEnv;
  platform?: string;
  inherit?: readonly string[];
  session?: boolean;
} = {}): NodeJS.ProcessEnv {
  const windows = platform === 'win32';
  const source = new Map(
    Object.entries(parent).map(([key, value]) => [windows ? key.toUpperCase() : key, value]),
  );
  const env: NodeJS.ProcessEnv = Object.create(null);
  const names = [
    'PATH',
    'HOME',
    'USER',
    'LOGNAME',
    'LANG',
    'LC_ALL',
    'LC_CTYPE',
    'TMPDIR',
    'TERM',
    ...(windows
      ? [
          'SystemRoot',
          'WINDIR',
          'COMSPEC',
          'PATHEXT',
          'TEMP',
          'TMP',
          'USERPROFILE',
          'APPDATA',
          'LOCALAPPDATA',
          'USERNAME',
          'USERDOMAIN',
          'COMPUTERNAME',
          'SESSIONNAME',
          'ALLUSERSPROFILE',
          'PROGRAMDATA',
          'ProgramFiles',
          'ProgramFiles(x86)',
          'ProgramW6432',
        ]
      : []),
    ...(session && !windows
      ? [
          'XDG_RUNTIME_DIR',
          'DBUS_SESSION_BUS_ADDRESS',
          'DISPLAY',
          'WAYLAND_DISPLAY',
          'XAUTHORITY',
          'XDG_SESSION_TYPE',
          'XDG_CURRENT_DESKTOP',
          'XDG_DATA_DIRS',
          'XDG_CONFIG_DIRS',
        ]
      : []),
    ...validateInheritedEnv([...inherit]),
  ];
  for (const key of names) {
    const value = source.get(windows ? key.toUpperCase() : key);
    if (value !== undefined) {
      const existing = windows
        ? Object.keys(env).find((name) => name.toUpperCase() === key.toUpperCase())
        : undefined;
      env[existing ?? key] = value;
    }
  }
  if (windows) {
    env.SystemRoot ||= env.WINDIR || 'C:\\Windows';
    env.WINDIR ||= env.SystemRoot;
    env.COMSPEC ||= path.win32.join(env.SystemRoot, 'System32', 'cmd.exe');
    const extensions = (env.PATHEXT || '').split(';').filter(Boolean);
    for (const extension of ['.COM', '.EXE', '.BAT', '.CMD']) {
      if (!extensions.some((value) => value.toUpperCase() === extension)) {
        extensions.push(extension);
      }
    }
    env.PATHEXT = extensions.join(';');
  }
  env.PATH ||= windows
    ? path.win32.join(env.SystemRoot!, 'System32')
    : '/usr/local/bin:/usr/bin:/bin';
  return env;
}

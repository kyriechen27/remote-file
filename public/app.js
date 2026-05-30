const state = {
  user: null,
  path: new URLSearchParams(location.search).get('path') || '/',
  view: 'files',
  entries: [],
  upload: null,
};

const $ = (id) => document.getElementById(id);

async function api(url, options = {}) {
  const res = await fetch(url, {
    credentials: 'same-origin',
    headers: options.body instanceof FormData ? {} : { 'content-type': 'application/json' },
    ...options,
  });
  if (!res.ok) {
    let message = '请求失败';
    try { message = (await res.json()).error || message; } catch {}
    throw new Error(message);
  }
  if (res.status === 204) return null;
  return res.json();
}

function toast(message) {
  $('toast').textContent = message;
  $('toast').classList.remove('hidden');
  clearTimeout(window.__toast);
  window.__toast = setTimeout(() => $('toast').classList.add('hidden'), 2600);
}

function show(view) {
  $('login').classList.toggle('hidden', view !== 'login');
  $('app').classList.toggle('hidden', view === 'login');
}

async function boot() {
  const session = await api('/api/session');
  if (!session.authenticated) {
    showSetupHint(session.fresh_setup);
    show('login');
    return;
  }
  state.user = session.user;
  $('account-name').textContent = session.user.username;
  $('role-label').textContent = session.user.role === 'admin' ? '管理员' : '文件系统';
  $('nav-admin').classList.toggle('hidden', session.user.role !== 'admin');
  if (!state.path || state.path === '/') {
    state.path = defaultPathForUser(session.user);
    history.replaceState(null, '', `/?path=${encodeURIComponent(state.path)}`);
  }
  show('app');
  await loadFiles();
}

function showSetupHint(freshSetup) {
  $('setup-hint').classList.toggle('hidden', !freshSetup);
  if (freshSetup) {
    $('login-user').value = 'admin';
    $('login-pass').value = 'admin';
  } else {
    $('login-user').value = '';
    $('login-pass').value = '';
  }
}

function defaultPathForUser(user) {
  return user.role === 'admin' ? '/' : `/user/${user.username}`;
}

function browseRoot() {
  const path = normalizeClientPath(state.path || defaultPathForUser(state.user));
  const roots = (state.user.permissions?.roots || [defaultPathForUser(state.user)])
    .map(normalizeClientPath)
    .filter((root) => root !== '/')
    .sort((a, b) => b.length - a.length);
  return roots.find((root) => path === root || path.startsWith(`${root}/`)) || defaultPathForUser(state.user);
}

function isAtBrowseRoot(path) {
  return normalizeClientPath(path) === browseRoot();
}

function normalizeClientPath(path) {
  const parts = path.split('/').filter(Boolean);
  return parts.length ? `/${parts.join('/')}` : '/';
}

function setView(view) {
  state.view = view;
  $('files-view').classList.toggle('hidden', view !== 'files');
  $('admin-view').classList.toggle('hidden', view !== 'admin');
  $('nav-files').classList.toggle('active', view === 'files');
  $('nav-admin').classList.toggle('active', view === 'admin');
  if (view === 'admin') {
    loadUsers();
    loadAuditLogs();
  }
}

function formatSize(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let size = bytes / 1024;
  let unit = 0;
  while (size >= 1024 && unit < units.length - 1) { size /= 1024; unit++; }
  return `${size.toFixed(size >= 10 ? 1 : 2)} ${units[unit]}`;
}

function icon(kind) {
  return kind === 'directory' ? '📁' : '📄';
}

function renderCrumbs(path) {
  const crumbs = $('crumbs');
  crumbs.innerHTML = '';
  const rootPath = browseRoot();
  const root = document.createElement('button');
  root.textContent = '全部文件';
  root.onclick = () => navigate(rootPath);
  crumbs.append(root);
  let current = rootPath === '/' ? '' : rootPath;
  const visibleParts = rootPath === '/'
    ? path.split('/').filter(Boolean)
    : path.slice(rootPath.length).split('/').filter(Boolean);
  visibleParts.forEach((part) => {
    current += `/${part}`;
    const sep = document.createElement('span');
    sep.textContent = '/';
    const btn = document.createElement('button');
    btn.textContent = part;
    btn.onclick = () => navigate(current);
    crumbs.append(sep, btn);
  });
}

async function loadFiles() {
  renderCrumbs(state.path);
  const data = await api(`/api/files?path=${encodeURIComponent(state.path)}`);
  state.entries = data.entries;
  const table = $('files-table');
  table.innerHTML = '<div class="row header"><span>名称</span><span>大小</span><span>下载</span><span>修改时间</span><span></span></div>';
  if (!isAtBrowseRoot(data.path)) {
    table.append(fileRow({ name: '..', path: parentPath(data.path), kind: 'directory', size: 0, modified: null }, true));
  }
  if (!data.entries.length) {
    const empty = document.createElement('div');
    empty.className = 'empty';
    empty.textContent = '这个目录还没有文件';
    table.append(empty);
  }
  data.entries.forEach((entry) => table.append(fileRow(entry)));
}

function fileRow(entry, up = false) {
  const row = document.createElement('div');
  row.className = 'row';
  const name = document.createElement('div');
  name.className = 'file-name';
  const glyph = document.createElement('span');
  glyph.textContent = up ? '↩' : icon(entry.kind);
  const open = document.createElement('button');
  open.textContent = entry.name;
  open.onclick = () => entry.kind === 'directory' ? navigate(entry.path) : location.assign(`/api/files${encodePath(entry.path)}`);
  name.append(glyph, open);

  const size = document.createElement('span');
  size.textContent = entry.kind === 'directory' ? '-' : formatSize(entry.size);
  const downloads = document.createElement('span');
  downloads.textContent = entry.kind === 'directory' ? '-' : String(entry.download_count || 0);
  const modified = document.createElement('span');
  modified.textContent = entry.modified ? new Date(entry.modified).toLocaleString() : '-';
  const actions = document.createElement('div');
  actions.className = 'actions';
  if (!up && entry.kind === 'file') {
    const download = document.createElement('button');
    download.textContent = '下载';
    download.onclick = () => location.assign(`${entry.download_url || `/api/files${encodePath(entry.path)}`}?download=1`);
    actions.append(download);
    const link = document.createElement('button');
    link.textContent = '复制链接';
    link.onclick = () => copyDownloadLink(entry);
    actions.append(link);
    if (entry.public) {
      const badge = document.createElement('button');
      badge.className = 'badge public';
      badge.textContent = '复制直链';
      badge.onclick = () => copyPublic(entry.public_url);
      actions.append(badge);
    }
    const grant = document.createElement('button');
    grant.textContent = entry.granted_users?.length ? `授权(${entry.granted_users.length})` : '授权';
    grant.onclick = () => grantFile(entry);
    actions.append(grant);
    const pub = document.createElement('button');
    pub.textContent = entry.public ? '取消公开' : '公开';
    pub.onclick = () => togglePublic(entry);
    actions.append(pub);
  }
  if (!up && state.user.permissions.can_delete) {
    const del = document.createElement('button');
    del.className = 'danger';
    del.textContent = '删除';
    del.onclick = () => deletePath(entry.path);
    actions.append(del);
  }
  row.append(name, size, downloads, modified, actions);
  return row;
}

function navigate(path) {
  const normalized = normalizeClientPath(path);
  state.path = normalized;
  history.replaceState(null, '', `/?path=${encodeURIComponent(normalized)}`);
  loadFiles().catch((err) => toast(err.message));
}

function parentPath(path) {
  if (isAtBrowseRoot(path)) return browseRoot();
  const parts = path.split('/').filter(Boolean);
  parts.pop();
  const parent = parts.length ? `/${parts.join('/')}` : '/';
  const root = browseRoot();
  if (root !== '/' && !parent.startsWith(`${root}/`) && parent !== root) {
    return root;
  }
  return parent;
}

function encodePath(path) {
  return path.split('/').filter(Boolean).map(encodeURIComponent).join('/').replace(/^/, '/');
}

function encodeApiPath(path) {
  const encoded = encodePath(path);
  return encoded === '/' ? '' : encoded;
}

function isPublicPath(path) {
  return path === '/public' || path.startsWith('/public/');
}

async function togglePublic(entry) {
  if (entry.public) {
    await api(`/api/admin/public${encodePath(entry.path)}`, { method: 'DELETE' });
    entry.public = false;
    entry.public_url = null;
    toast('已取消公开');
    if (isPublicPath(state.path)) {
      state.entries = (state.entries || []).filter((item) => item.path !== entry.path);
    }
  } else {
    const res = await api(`/api/admin/public${encodePath(entry.path)}`, { method: 'PUT' });
    entry.public = true;
    entry.public_url = res.public_url;
    copyPublic(res.public_url).catch(() => {
      toast(`已公开：${location.origin}${res.public_url}`);
    });
  }
  await loadFiles();
}

async function copyDownloadLink(entry) {
  const base = entry.download_url || `/api/files${encodePath(entry.path)}`;
  const url = `${location.origin}${base}${base.includes('?') ? '&' : '?'}download=1`;
  await copyText(url);
  toast('登录链接已复制');
}

async function copyPublic(publicUrl) {
  const url = `${location.origin}${publicUrl}`;
  await copyText(url);
  toast('直链已复制');
}

async function copyText(url) {
  if (navigator.clipboard && window.isSecureContext) {
    await navigator.clipboard.writeText(url);
  } else {
    fallbackCopy(url);
  }
}

function fallbackCopy(text) {
  const input = document.createElement('textarea');
  input.value = text;
  input.setAttribute('readonly', '');
  input.style.position = 'fixed';
  input.style.left = '-9999px';
  input.style.top = '0';
  document.body.append(input);
  input.select();
  const copied = document.execCommand('copy');
  input.remove();
  if (!copied) throw new Error('复制失败');
}

async function grantFile(entry) {
  const current = (entry.granted_users || []).join(', ');
  const value = prompt('输入要授权的用户名，多个用户用逗号分隔。留空会清空授权。', current);
  if (value === null) return;
  const users = value.split(',').map((name) => name.trim()).filter(Boolean);
  const res = await api(`/api/grants${encodePath(entry.path)}`, {
    method: 'PUT',
    body: JSON.stringify({ users }),
  });
  entry.granted_users = res.granted_users;
  toast(users.length ? '授权已更新' : '授权已清空');
  await loadFiles();
}

async function deletePath(path) {
  if (!confirm(`删除 ${path}？`)) return;
  await api(`/api/files${encodePath(path)}`, { method: 'DELETE' });
  toast('已删除');
  await loadFiles();
}

async function uploadFiles(files) {
  if (!files.length) return;
  const fileList = [...files];
  const existing = new Set((state.entries || []).map((entry) => entry.name));
  const hasConflict = fileList.some((file) => existing.has(file.name));
  let conflict = 'error';
  if (hasConflict) {
    conflict = confirm('检测到同名文件。确定覆盖已有文件？取消则自动改名上传。')
      ? 'overwrite'
      : 'rename';
  }
  const data = new FormData();
  fileList.forEach((file) => data.append('file', file));
  showUploadProgress(fileList);
  $('upload-btn').disabled = true;
  try {
    await uploadWithProgress(
      `/api/files${encodeApiPath(state.path)}?conflict=${conflict}`,
      data,
      (event) => updateUploadProgress(event, fileList)
    );
    updateUploadProgress({ lengthComputable: true, loaded: totalUploadSize(fileList), total: totalUploadSize(fileList) }, fileList);
    toast('上传完成');
    await loadFiles();
  } finally {
    $('upload-btn').disabled = false;
    $('upload-input').value = '';
    setTimeout(hideUploadProgress, 500);
  }
}

function uploadWithProgress(url, data, onProgress) {
  return new Promise((resolve, reject) => {
    const request = new XMLHttpRequest();
    request.open('PUT', url);
    request.withCredentials = true;
    request.upload.onprogress = onProgress;
    request.onload = () => {
      if (request.status >= 200 && request.status < 300) {
        resolve();
        return;
      }
      reject(new Error(uploadErrorMessage(request)));
    };
    request.onerror = () => reject(new Error('上传失败，请检查网络后重试'));
    request.onabort = () => reject(new Error('上传已取消'));
    request.send(data);
  });
}

function uploadErrorMessage(request) {
  const fallback = `上传失败（HTTP ${request.status || '网络错误'}）`;
  try {
    return JSON.parse(request.responseText).error || fallback;
  } catch {
    return request.responseText || fallback;
  }
}

function showUploadProgress(files) {
  state.upload = {
    startedAt: performance.now(),
    lastAt: performance.now(),
    lastLoaded: 0,
    speed: 0,
    saveTimer: null,
  };
  $('upload-progress').classList.remove('hidden');
  $('upload-progress-title').textContent = `正在上传 ${files.length} 个文件`;
  $('upload-progress-detail').textContent = '准备上传';
  $('upload-progress-bar').style.width = '0%';
}

function updateUploadProgress(event, files) {
  const total = event.lengthComputable ? event.total : totalUploadSize(files);
  const loaded = event.lengthComputable ? event.loaded : 0;
  if (!total) {
    $('upload-progress-detail').textContent = `正在上传 · ${formatUploadSpeed(loaded)}`;
    return;
  }
  const percent = Math.min(100, Math.round((loaded / total) * 100));
  if (percent >= 100) {
    showUploadSaving(loaded, total);
  } else {
    $('upload-progress-title').textContent = `正在上传 ${files.length} 个文件`;
    $('upload-progress-detail').textContent = `${percent}% · ${formatSize(loaded)} / ${formatSize(total)} · ${formatUploadSpeed(loaded)}`;
  }
  $('upload-progress-bar').style.width = `${percent}%`;
}

function hideUploadProgress() {
  $('upload-progress').classList.add('hidden');
  if (state.upload?.saveTimer) clearTimeout(state.upload.saveTimer);
  state.upload = null;
}

function totalUploadSize(files) {
  return files.reduce((total, file) => total + file.size, 0);
}

function formatUploadSpeed(loaded) {
  const now = performance.now();
  const upload = state.upload || { startedAt: now, lastAt: now, lastLoaded: loaded, speed: 0 };
  const elapsedSinceLast = Math.max(0, now - upload.lastAt);
  if (elapsedSinceLast >= 250 && loaded >= upload.lastLoaded) {
    const instantSpeed = ((loaded - upload.lastLoaded) * 1000) / elapsedSinceLast;
    upload.speed = upload.speed ? upload.speed * 0.65 + instantSpeed * 0.35 : instantSpeed;
    upload.lastAt = now;
    upload.lastLoaded = loaded;
    state.upload = upload;
  }
  const elapsed = Math.max(1, now - upload.startedAt);
  const averageSpeed = (loaded * 1000) / elapsed;
  const speed = upload.speed || averageSpeed;
  return `${formatSize(speed)}/s`;
}

function showUploadSaving(loaded, total) {
  $('upload-progress-title').textContent = '服务器保存中';
  $('upload-progress-detail').textContent = `100% · ${formatSize(loaded)} / ${formatSize(total)} · 等待服务器确认`;
  if (state.upload && !state.upload.saveTimer) {
    state.upload.saveTimer = setTimeout(() => {
      $('upload-progress-detail').textContent = `100% · ${formatSize(loaded)} / ${formatSize(total)} · 仍在等待服务器保存，请稍候`;
    }, 15000);
  }
}

async function createFolder() {
  const name = prompt('输入文件夹名称');
  if (name === null) return;
  const trimmed = name.trim();
  if (!trimmed) return;
  await api(`/api/files${encodeApiPath(state.path)}`, {
    method: 'POST',
    body: JSON.stringify({ name: trimmed }),
  });
  toast('文件夹已创建');
  await loadFiles();
}

async function loadUsers() {
  const users = await api('/api/admin/users');
  const table = $('users-table');
  table.innerHTML = '<div class="row header"><span>用户</span><span>角色</span><span>目录</span><span></span></div>';
  users.forEach((user) => {
    const row = document.createElement('div');
    row.className = 'row';
    row.innerHTML = `<strong>${user.username}</strong><span>${user.role}</span><span>${user.permissions.roots.join(', ')}</span>`;
    const actions = document.createElement('div');
    actions.className = 'actions';
    const edit = document.createElement('button');
    edit.textContent = '编辑';
    edit.onclick = () => fillUser(user);
    actions.append(edit);
    const del = document.createElement('button');
    del.className = 'danger';
    del.textContent = '删除';
    del.onclick = async () => {
      await api(`/api/admin/users/${encodeURIComponent(user.username)}`, { method: 'DELETE' });
      toast('用户已删除');
      loadUsers();
    };
    actions.append(del);
    row.append(actions);
    table.append(row);
  });
}

async function loadAuditLogs() {
  const logs = await api('/api/admin/audit');
  const table = $('audit-table');
  table.innerHTML = '<div class="row header audit"><span>时间</span><span>用户</span><span>动作</span><span>路径</span><span>结果</span></div>';
  if (!logs.length) {
    const empty = document.createElement('div');
    empty.className = 'empty';
    empty.textContent = '暂无活动记录';
    table.append(empty);
    return;
  }
  logs.forEach((log) => {
    const row = document.createElement('div');
    row.className = 'row audit';
    const time = document.createElement('span');
    time.textContent = new Date(log.at).toLocaleString();
    const user = document.createElement('span');
    user.textContent = log.username || '-';
    const action = document.createElement('span');
    action.textContent = auditActionLabel(log.action);
    const path = document.createElement('span');
    path.textContent = log.path || log.ip || '-';
    const status = document.createElement('span');
    status.textContent = auditResultLabel(log);
    row.append(time, user, action, path, status);
    table.append(row);
  });
}

function auditActionLabel(action) {
  return ({
    login: '登录',
    logout: '退出登录',
    download: '下载文件',
    public_download: '公开下载',
    upload: '上传文件',
    mkdir: '新建文件夹',
    delete: '删除',
    publish: '公开文件',
    unpublish: '取消公开',
    grant: '文件授权',
    create_user: '创建用户',
    update_user: '修改用户',
    delete_user: '删除用户',
  })[action] || action;
}

function auditResultLabel(log) {
  const code = Number(log.status);
  const outcome = code >= 200 && code < 300 ? '成功' : '失败';
  return log.detail ? `${outcome}：${log.detail}` : outcome;
}

function fillUser(user) {
  $('edit-username').value = user.username;
  $('user-username').value = user.username;
  $('user-username').disabled = false;
  $('user-password').value = '';
  $('user-role').value = user.role;
  $('user-roots').value = user.permissions.roots.join(', ');
  $('perm-upload').checked = user.permissions.can_upload;
  $('perm-delete').checked = user.permissions.can_delete;
  $('perm-publish').checked = user.permissions.can_publish;
}

function resetUserForm() {
  $('edit-username').value = '';
  $('user-username').disabled = false;
  $('user-form').reset();
  $('user-role').value = 'user';
  $('user-roots').value = '';
  $('perm-upload').checked = true;
  $('perm-delete').checked = true;
  $('perm-publish').checked = true;
  updateDefaultUserRoot();
}

function updateDefaultUserRoot() {
  if ($('edit-username').value) return;
  if ($('user-role').value === 'admin') {
    $('user-roots').value = '/';
    return;
  }
  const username = $('user-username').value.trim();
  $('user-roots').value = username ? `/user/${username}` : '';
}

async function saveUser(event) {
  event.preventDefault();
  const editing = $('edit-username').value;
  const payload = {
    username: $('user-username').value.trim(),
    password: $('user-password').value,
    role: $('user-role').value,
    roots: $('user-roots').value.split(',').map((v) => v.trim()).filter(Boolean),
    can_upload: $('perm-upload').checked,
    can_delete: $('perm-delete').checked,
    can_publish: $('perm-publish').checked,
  };
  await api(editing ? `/api/admin/users/${encodeURIComponent(editing)}` : '/api/admin/users', {
    method: editing ? 'PUT' : 'POST',
    body: JSON.stringify(payload),
  });
  toast(editing ? '用户已保存' : '用户已创建');
  resetUserForm();
  await loadUsers();
}

$('login-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  try {
    const session = await api('/api/login', {
      method: 'POST',
      body: JSON.stringify({ username: $('login-user').value, password: $('login-pass').value }),
    });
    state.user = session.user;
    await boot();
  } catch (err) {
    toast(err.message);
  }
});
$('logout').onclick = async () => { await api('/api/logout', { method: 'POST' }); location.reload(); };
$('nav-files').onclick = () => setView('files');
$('nav-admin').onclick = () => setView('admin');
$('refresh-btn').onclick = () => loadFiles().catch((err) => toast(err.message));
$('upload-btn').onclick = () => $('upload-input').click();
$('mkdir-btn').onclick = () => createFolder().catch((err) => toast(err.message));
$('upload-input').onchange = (event) => uploadFiles(event.target.files).catch((err) => toast(err.message));
$('user-form').addEventListener('submit', (event) => saveUser(event).catch((err) => toast(err.message)));
$('reset-user-form').onclick = resetUserForm;
$('user-username').addEventListener('input', updateDefaultUserRoot);
$('user-role').addEventListener('change', updateDefaultUserRoot);

boot().catch((err) => toast(err.message));

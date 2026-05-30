const DEFAULT_ADMIN = 'admin';
const DEFAULT_PASSWORD = 'admin';
const STATE_KEY = '__remote_file_meta/state.json';
const AUDIT_KEY = '__remote_file_meta/audit.json';
const MULTIPART_PART_SIZE = 32 * 1024 * 1024;

export async function onRequest(context) {
  const app = new RemoteFileApp(context);
  return app.handle();
}

class RemoteFileApp {
  constructor(context) {
    this.context = context;
    this.request = context.request;
    this.env = context.env || {};
    this.url = new URL(this.request.url);
    this.bucket = this.env.BUCKET;
  }

  async handle() {
    try {
      const method = this.request.method.toUpperCase();
      const pathname = this.url.pathname;
      if (!isFunctionRoute(pathname)) {
        return this.context.next();
      }
      if (!this.bucket) {
        return jsonError(500, 'Cloudflare R2 binding BUCKET is not configured');
      }
      if (method === 'GET' && pathname === '/api/session') return this.apiSession();
      if (method === 'POST' && pathname === '/api/login') return this.apiLogin();
      if (method === 'POST' && pathname === '/api/logout') return this.apiLogout();
      if (pathname === '/api/uploads/start' && method === 'POST') return this.apiMultipartStart();
      if (pathname === '/api/uploads/part' && method === 'PUT') return this.apiMultipartPart();
      if (pathname === '/api/uploads/complete' && method === 'POST') return this.apiMultipartComplete();
      if (pathname === '/api/uploads/abort' && method === 'DELETE') return this.apiMultipartAbort();
      if (pathname === '/api/files') {
        if (method === 'GET') return this.apiFiles('/');
        if (method === 'PUT') return this.apiUpload('/');
        if (method === 'POST') return this.apiMkdir('/');
      }
      if (pathname.startsWith('/api/files/')) {
        const path = decodeRoutePath(pathname.slice('/api/files'.length));
        if (method === 'GET') return this.apiDownload(path);
        if (method === 'PUT') return this.apiUpload(path);
        if (method === 'POST') return this.apiMkdir(path);
        if (method === 'DELETE') return this.apiDelete(path);
      }
      if (pathname === '/api/admin/users') {
        if (method === 'GET') return this.apiUsers();
        if (method === 'POST') return this.apiCreateUser();
      }
      if (pathname === '/api/admin/audit' && method === 'GET') return this.apiAudit();
      if (pathname.startsWith('/api/admin/users/')) {
        const username = decodeURIComponent(pathname.slice('/api/admin/users/'.length));
        if (method === 'PUT') return this.apiUpdateUser(username);
        if (method === 'DELETE') return this.apiDeleteUser(username);
      }
      if (pathname.startsWith('/api/admin/public/')) {
        const path = decodeRoutePath(pathname.slice('/api/admin/public'.length));
        if (method === 'PUT') return this.apiPublish(path);
        if (method === 'DELETE') return this.apiUnpublish(path);
      }
      if (pathname.startsWith('/api/grants/')) {
        const path = decodeRoutePath(pathname.slice('/api/grants'.length));
        if (method === 'PUT') return this.apiGrant(path);
      }
      if (pathname.startsWith('/public/') && method === 'GET') return this.publicDownload(pathname.slice('/public'.length));
      if (pathname.startsWith('/p/') && method === 'GET') return this.publicDownload(pathname.slice('/p'.length));
      return jsonError(404, 'Not found');
    } catch (error) {
      return jsonError(500, error && error.message ? error.message : String(error));
    }
  }

  async apiSession() {
    const store = await this.loadStore();
    const user = await this.currentUser(store);
    return json({ authenticated: Boolean(user), user: user ? userView(user) : null, fresh_setup: store.fresh_setup });
  }

  async apiLogin() {
    const payload = await readJson(this.request);
    const username = String(payload.username || '');
    const password = String(payload.password || '');
    const store = await this.loadStore();
    const user = store.users[username];
    if (!user || !(await verifyPassword(password, user.password_hash))) {
      await this.recordAudit(store, username || null, 'login', null, 401, user ? '密码错误' : '用户名不存在');
      return jsonError(401, '用户名或密码错误');
    }
    const token = randomToken();
    store.sessions[token] = { username, created_at: nowIso() };
    if (store.fresh_setup) store.fresh_setup = false;
    await this.saveStore(store);
    await this.recordAudit(store, username, 'login', null, 200, '登录成功');
    return json({ authenticated: true, user: userView(user), fresh_setup: store.fresh_setup }, {
      'set-cookie': `rf_session=${token}; HttpOnly; SameSite=Lax; Secure; Path=/; Max-Age=604800`,
    });
  }

  async apiLogout() {
    const store = await this.loadStore();
    const token = sessionToken(this.request.headers);
    let username = null;
    if (token && store.sessions[token]) {
      username = store.sessions[token].username;
      delete store.sessions[token];
      await this.saveStore(store);
    }
    await this.recordAudit(store, username, 'logout', null, 204, '退出登录');
    return new Response(null, { status: 204, headers: { 'set-cookie': 'rf_session=; HttpOnly; SameSite=Lax; Secure; Path=/; Max-Age=0' } });
  }

  async apiFiles(rootPath) {
    const store = await this.loadStore();
    const user = await this.currentUserOrBasic(store);
    if (!user) return jsonError(401, '请先登录');
    const path = normalizePath(this.url.searchParams.get('path') || rootPath);
    if (!canRead(user, path)) return jsonError(403, '没有访问此目录的权限');
    const entries = await this.listEntries(store, user, path);
    return json({ path, entries });
  }

  async apiDownload(path) {
    const store = await this.loadStore();
    const user = await this.currentUserOrBasic(store);
    const normalized = normalizePath(path);
    if (!user) {
      await this.recordAudit(store, null, 'download', normalized, 401, '未认证');
      return basicAuthRequired();
    }
    if (!canAccessPath(store, user, normalized)) {
      await this.recordAudit(store, user.username, 'download', normalized, 403, '下载权限不足');
      return basicAuthRequired();
    }
    const response = await this.serveCountedFile(store, normalized, this.forceDownload());
    await this.recordAudit(store, user.username, 'download', normalized, response.status, '登录下载');
    return response;
  }

  async publicDownload(path) {
    const store = await this.loadStore();
    const publicPath = normalizePublicRequestPath(path);
    const sourcePath = publicSourcePath(store, publicPath);
    if (!sourcePath) {
      await this.recordAudit(store, null, 'public_download', publicPath, 404, '公开文件不存在');
      return jsonError(404, '公开文件不存在');
    }
    const response = await this.serveCountedPublicFile(store, sourcePath, publicPath, this.forceDownload());
    await this.recordAudit(store, null, 'public_download', publicPath, response.status, '公开下载');
    return response;
  }

  async apiUpload(path) {
    const store = await this.loadStore();
    const user = await this.currentUserOrBasic(store);
    const dirPath = normalizePath(path);
    if (!user) return jsonError(401, '请先登录');
    if (!user.permissions.can_upload) {
      await this.recordAudit(store, user.username, 'upload', dirPath, 403, '上传权限不足');
      return jsonError(403, '没有上传权限');
    }
    if (!canRead(user, dirPath)) {
      await this.recordAudit(store, user.username, 'upload', dirPath, 403, '上传目录权限不足');
      return jsonError(403, '不能上传到这个目录');
    }
    const form = await this.request.formData();
    const conflict = this.url.searchParams.get('conflict') || 'error';
    let uploaded = 0;
    for (const value of form.values()) {
      if (!(value instanceof File)) continue;
      const cleanName = cleanFileName(value.name || 'upload.bin');
      const targetPath = await this.uploadTargetPath(dirPath, cleanName, conflict);
      if (targetPath.response) return targetPath.response;
      await this.bucket.put(fileKey(targetPath.path), value.stream(), {
        httpMetadata: { contentType: value.type || contentTypeForPath(targetPath.path) },
        customMetadata: { remoteFilePath: targetPath.path },
      });
      await this.putDirectoryMarker(parentPath(targetPath.path));
      uploaded += 1;
    }
    await this.recordAudit(store, user.username, 'upload', dirPath, 204, `上传 ${uploaded} 个文件`);
    return new Response(null, { status: 204 });
  }

  async apiMultipartStart() {
    const store = await this.loadStore();
    const payload = await readJson(this.request);
    const dirPath = normalizePath(payload.path || '/');
    const result = await this.requireUploadAccess(store, dirPath);
    if (result.response) return result.response;
    const cleanName = cleanFileName(payload.file_name || 'upload.bin');
    const conflict = payload.conflict || 'error';
    const targetPath = await this.uploadTargetPath(dirPath, cleanName, conflict);
    if (targetPath.response) return targetPath.response;
    const upload = await this.bucket.createMultipartUpload(fileKey(targetPath.path), {
      httpMetadata: { contentType: payload.content_type || contentTypeForPath(targetPath.path) },
      customMetadata: {
        remoteFilePath: targetPath.path,
        uploadedBy: result.user.username,
        originalName: cleanName,
      },
    });
    return json({
      path: targetPath.path,
      upload_id: upload.uploadId,
      part_size: MULTIPART_PART_SIZE,
    });
  }

  async apiMultipartPart() {
    const store = await this.loadStore();
    const path = normalizePath(this.url.searchParams.get('path') || '/');
    const uploadId = this.url.searchParams.get('upload_id') || '';
    const partNumber = Number(this.url.searchParams.get('part_number') || '0');
    const result = await this.requireUploadAccess(store, parentPath(path));
    if (result.response) return result.response;
    if (!uploadId || !Number.isInteger(partNumber) || partNumber < 1 || partNumber > 10000) {
      return jsonError(400, '分片参数不合法');
    }
    const upload = this.bucket.resumeMultipartUpload(fileKey(path), uploadId);
    const part = await upload.uploadPart(partNumber, this.request.body);
    return json(part);
  }

  async apiMultipartComplete() {
    const store = await this.loadStore();
    const payload = await readJson(this.request);
    const path = normalizePath(payload.path || '/');
    const uploadId = String(payload.upload_id || '');
    const parts = Array.isArray(payload.parts) ? payload.parts : [];
    const result = await this.requireUploadAccess(store, parentPath(path));
    if (result.response) return result.response;
    if (!uploadId || !parts.length) return jsonError(400, '缺少上传分片');
    const upload = this.bucket.resumeMultipartUpload(fileKey(path), uploadId);
    const completed = await upload.complete(parts.map((part) => ({
      partNumber: Number(part.partNumber),
      etag: String(part.etag),
    })));
    await this.putDirectoryMarker(parentPath(path));
    await this.recordAudit(store, result.user.username, 'upload', parentPath(path), 204, `分片上传 ${path}`);
    return json({ path, etag: completed?.etag || null });
  }

  async apiMultipartAbort() {
    const store = await this.loadStore();
    const path = normalizePath(this.url.searchParams.get('path') || '/');
    const uploadId = this.url.searchParams.get('upload_id') || '';
    const result = await this.requireUploadAccess(store, parentPath(path));
    if (result.response) return result.response;
    if (!uploadId) return jsonError(400, '缺少 upload_id');
    const upload = this.bucket.resumeMultipartUpload(fileKey(path), uploadId);
    await upload.abort();
    return new Response(null, { status: 204 });
  }

  async apiMkdir(parent) {
    const store = await this.loadStore();
    const user = await this.currentUser(store);
    const parentPathValue = normalizePath(parent);
    if (!user) return jsonError(401, '请先登录');
    if (!user.permissions.can_upload) {
      await this.recordAudit(store, user.username, 'mkdir', parentPathValue, 403, '创建文件夹权限不足');
      return jsonError(403, '没有创建文件夹权限');
    }
    if (!canRead(user, parentPathValue)) return jsonError(403, '不能在这个目录创建文件夹');
    const payload = await readJson(this.request);
    const name = cleanDirectoryName(payload.name || '');
    if (!name) return jsonError(400, '文件夹名称不合法');
    const target = joinVirtual(parentPathValue, name);
    if (!canRead(user, target)) return jsonError(403, '不能创建这个文件夹');
    if (await this.pathExists(target)) return jsonError(409, '文件夹已存在');
    await this.putDirectoryMarker(target);
    await this.recordAudit(store, user.username, 'mkdir', target, 201, '创建文件夹');
    return json({ path: target }, {}, 201);
  }

  async apiDelete(path) {
    const store = await this.loadStore();
    const user = await this.currentUser(store);
    const normalized = normalizePath(path);
    if (!user) return jsonError(401, '请先登录');
    if (!user.permissions.can_delete) {
      await this.recordAudit(store, user.username, 'delete', normalized, 403, '删除权限不足');
      return jsonError(403, '没有删除权限');
    }
    if (!canRead(user, normalized)) return jsonError(403, '不能删除这个文件');
    const deleted = await this.deletePath(normalized);
    if (!deleted) return jsonError(404, '文件不存在');
    const publicPath = publicPathForSource(store, normalized);
    if (publicPath) {
      delete store.public_files[publicPath];
      delete store.download_counts[publicPath];
    }
    delete store.public_files[normalized];
    delete store.public_copies[normalized];
    delete store.file_grants[normalized];
    delete store.download_counts[normalized];
    await this.saveStore(store);
    await this.recordAudit(store, user.username, 'delete', normalized, 204, '删除文件或目录');
    return new Response(null, { status: 204 });
  }

  async apiUsers() {
    const result = await this.requireAdmin();
    if (result.response) return result.response;
    return json(Object.values(result.store.users).sort((a, b) => a.username.localeCompare(b.username)).map(userView));
  }

  async apiAudit() {
    const result = await this.requireAdmin();
    if (result.response) return result.response;
    const logs = await this.loadAuditLogs();
    return json(logs.slice(-200).reverse());
  }

  async apiCreateUser() {
    const result = await this.requireAdmin();
    if (result.response) return result.response;
    const { store, user: admin } = result;
    const payload = await readJson(this.request);
    if (!String(payload.username || '').trim() || !String(payload.password || '').trim()) return jsonError(400, '用户名和密码不能为空');
    if (store.users[payload.username]) return jsonError(409, '用户已存在');
    const user = await userFromPayload(payload);
    store.users[user.username] = user;
    for (const root of user.permissions.roots) await this.putDirectoryMarker(root);
    await this.saveStore(store);
    await this.recordAudit(store, admin.username, 'create_user', null, 201, `创建用户 ${user.username}`);
    return json(userView(user), {}, 201);
  }

  async apiUpdateUser(username) {
    const result = await this.requireAdmin();
    if (result.response) return result.response;
    const { store, user: admin } = result;
    const payload = await readJson(this.request);
    const newUsername = String(payload.username || '').trim();
    if (!newUsername) return jsonError(400, '用户名不能为空');
    if (newUsername !== username && store.users[newUsername]) return jsonError(409, '用户名已存在');
    const existing = store.users[username];
    if (!existing) return jsonError(404, '用户不存在');
    if (existing.role === 'admin' && payload.role !== 'admin' && Object.values(store.users).filter((u) => u.role === 'admin').length <= 1) {
      return jsonError(400, '至少需要保留一个管理员');
    }
    delete store.users[username];
    existing.username = newUsername;
    existing.role = payload.role === 'admin' ? 'admin' : 'user';
    existing.permissions = permissionsFromPayload(payload);
    if (payload.password) existing.password_hash = await hashPassword(String(payload.password));
    for (const session of Object.values(store.sessions)) {
      if (session.username === username) session.username = newUsername;
    }
    for (const root of existing.permissions.roots) await this.putDirectoryMarker(root);
    store.users[newUsername] = existing;
    await this.saveStore(store);
    await this.recordAudit(store, admin.username, 'update_user', null, 200, `修改用户 ${existing.username}`);
    return json(userView(existing));
  }

  async apiDeleteUser(username) {
    const result = await this.requireAdmin();
    if (result.response) return result.response;
    const { store, user: admin } = result;
    const user = store.users[username];
    if (!user) return jsonError(404, '用户不存在');
    if (user.role === 'admin' && Object.values(store.users).filter((u) => u.role === 'admin').length <= 1) {
      return jsonError(400, '至少需要保留一个管理员');
    }
    delete store.users[username];
    for (const [token, session] of Object.entries(store.sessions)) {
      if (session.username === username) delete store.sessions[token];
    }
    await this.saveStore(store);
    await this.recordAudit(store, admin.username, 'delete_user', null, 204, `删除用户 ${username}`);
    return new Response(null, { status: 204 });
  }

  async apiPublish(path) {
    const store = await this.loadStore();
    const user = await this.currentUser(store);
    const normalized = normalizePath(path);
    if (!user) return jsonError(401, '请先登录');
    if (!user.permissions.can_publish) return jsonError(403, '没有公开文件权限');
    if (!canRead(user, normalized)) return jsonError(403, '不能公开这个文件');
    const head = await this.bucket.head(fileKey(normalized));
    if (!head) return jsonError(404, '文件不存在');
    if (head.customMetadata && head.customMetadata.directory === 'true') return jsonError(400, '暂不支持公开整个目录');
    delete store.public_copies[normalized];
    store.public_files[normalized] = { path: normalized, published_by: user.username, published_at: nowIso() };
    await this.saveStore(store);
    await this.recordAudit(store, user.username, 'publish', normalized, 200, '公开文件链接');
    return json({ public_path: normalized, public_url: publicUrlForSourcePath(normalized) });
  }

  async apiUnpublish(path) {
    const store = await this.loadStore();
    const user = await this.currentUser(store);
    const normalized = normalizePath(path);
    if (!user) return jsonError(401, '请先登录');
    const publicPath = publicPathForSource(store, normalized) || normalized;
    delete store.public_copies[normalized];
    delete store.public_files[publicPath];
    delete store.download_counts[publicPath];
    await this.saveStore(store);
    await this.recordAudit(store, user.username, 'unpublish', normalized, 200, `取消公开链接 ${publicPath}`);
    return json({ public_path: publicPath });
  }

  async apiGrant(path) {
    const store = await this.loadStore();
    const user = await this.currentUser(store);
    const normalized = normalizePath(path);
    if (!user) return jsonError(401, '请先登录');
    const head = await this.bucket.head(fileKey(normalized));
    if (!head) return jsonError(404, '文件不存在');
    if (!canAccessPath(store, user, normalized)) return jsonError(403, '不能授权这个文件');
    const payload = await readJson(this.request);
    const grantedUsers = [];
    for (const value of payload.users || []) {
      const name = String(value).trim();
      if (!name || name === user.username) continue;
      if (!store.users[name]) return jsonError(400, `用户 ${name} 不存在`);
      if (!grantedUsers.includes(name)) grantedUsers.push(name);
    }
    grantedUsers.sort();
    if (grantedUsers.length) {
      store.file_grants[normalized] = { path: normalized, users: grantedUsers, updated_by: user.username, updated_at: nowIso() };
    } else {
      delete store.file_grants[normalized];
    }
    await this.saveStore(store);
    await this.recordAudit(store, user.username, 'grant', normalized, 200, grantedUsers.length ? `授权给 ${grantedUsers.join(', ')}` : '清空授权');
    return json({ download_url: `/api/files${encodePath(normalized)}`, granted_users: grantedUsers });
  }

  async requireAdmin() {
    const store = await this.loadStore();
    const user = await this.currentUser(store);
    if (!user) return { response: jsonError(401, '请先登录') };
    if (user.role !== 'admin') return { response: jsonError(403, '只有管理员可以访问后台') };
    return { store, user };
  }

  async requireUploadAccess(store, dirPath) {
    const user = await this.currentUserOrBasic(store);
    if (!user) return { response: jsonError(401, '请先登录') };
    if (!user.permissions.can_upload) return { response: jsonError(403, '没有上传权限') };
    if (!canRead(user, dirPath)) return { response: jsonError(403, '不能上传到这个目录') };
    return { user };
  }

  async listEntries(store, user, path) {
    const prefix = filePrefix(path);
    const listed = await this.bucket.list({ prefix, delimiter: '/' });
    const entries = [];
    for (const item of listed.delimitedPrefixes || []) {
      const entryPath = pathFromFileKey(item.replace(/\/$/, ''));
      if (entryPath === path || !canAccessPath(store, user, entryPath)) continue;
      const name = basename(entryPath);
      entries.push(fileEntry(store, { name, path: entryPath, kind: 'directory', size: 0, modified: null }));
    }
    for (const object of listed.objects || []) {
      if (object.key.endsWith('/.dir')) continue;
      const entryPath = pathFromFileKey(object.key);
      if (entryPath === path || !canAccessPath(store, user, entryPath)) continue;
      const name = basename(entryPath);
      entries.push(fileEntry(store, { name, path: entryPath, kind: 'file', size: object.size || 0, modified: object.uploaded ? new Date(object.uploaded).toISOString() : null }));
    }
    entries.sort((a, b) => {
      if (a.kind !== b.kind) return a.kind === 'directory' ? -1 : 1;
      return a.name.toLowerCase().localeCompare(b.name.toLowerCase());
    });
    return entries;
  }

  async uploadTargetPath(dirPath, fileName, conflict) {
    let target = joinVirtual(dirPath, fileName);
    if (!(await this.pathExists(target))) return { path: target };
    if (conflict === 'overwrite') return { path: target };
    if (conflict !== 'rename') return { response: jsonError(409, '文件已存在') };
    const dot = fileName.lastIndexOf('.');
    const stem = dot > 0 ? fileName.slice(0, dot) : fileName;
    const ext = dot > 0 ? fileName.slice(dot) : '';
    for (let index = 1; index < 1000; index += 1) {
      target = joinVirtual(dirPath, `${stem}-${index}${ext}`);
      if (!(await this.pathExists(target))) return { path: target };
    }
    return { response: jsonError(500, '无法生成可用文件名') };
  }

  async pathExists(path) {
    if (await this.bucket.head(fileKey(path))) return true;
    const listed = await this.bucket.list({ prefix: filePrefix(path), limit: 1 });
    return (listed.objects || []).length > 0;
  }

  async putDirectoryMarker(path) {
    const normalized = normalizePath(path);
    await this.bucket.put(`${filePrefix(normalized)}.dir`, '', { customMetadata: { directory: 'true' } });
  }

  async deletePath(path) {
    const key = fileKey(path);
    const head = await this.bucket.head(key);
    if (head) {
      await this.bucket.delete(key);
      return true;
    }
    const prefix = filePrefix(path);
    let cursor;
    let deleted = 0;
    do {
      const listed = await this.bucket.list({ prefix, cursor });
      const keys = (listed.objects || []).map((object) => object.key);
      if (keys.length) {
        await Promise.all(keys.map((item) => this.bucket.delete(item)));
        deleted += keys.length;
      }
      cursor = listed.truncated ? listed.cursor : undefined;
    } while (cursor);
    return deleted > 0;
  }

  async serveCountedFile(store, path, forceDownload) {
    const response = await this.serveFile(path, forceDownload);
    if (response.status >= 200 && response.status < 300) {
      store.download_counts[path] = (store.download_counts[path] || 0) + 1;
      await this.saveStore(store);
    }
    return response;
  }

  async serveCountedPublicFile(store, sourcePath, publicPath, forceDownload) {
    const response = await this.serveFile(sourcePath, forceDownload);
    if (response.status >= 200 && response.status < 300) {
      store.download_counts[publicPath] = (store.download_counts[publicPath] || 0) + 1;
      if (sourcePath !== publicPath) store.download_counts[sourcePath] = (store.download_counts[sourcePath] || 0) + 1;
      await this.saveStore(store);
    }
    return response;
  }

  async serveFile(path, forceDownload) {
    const object = await this.bucket.get(fileKey(path));
    if (!object) {
      const listed = await this.bucket.list({ prefix: filePrefix(path), limit: 1 });
      if ((listed.objects || []).length) {
        return Response.redirect(`${this.url.origin}/?path=${encodeURIComponent(normalizePath(path))}`, 302);
      }
      return jsonError(404, '文件不存在');
    }
    const headers = new Headers();
    object.writeHttpMetadata(headers);
    if (!headers.has('content-type')) headers.set('content-type', contentTypeForPath(path));
    headers.set('content-disposition', `${forceDownload ? 'attachment' : 'inline'}; filename="${basename(path).replaceAll('"', '') || 'download'}"`);
    return new Response(object.body, { headers });
  }

  forceDownload() {
    const value = this.url.searchParams.get('download');
    return value === '1' || value === 'true';
  }

  async currentUser(store) {
    const token = sessionToken(this.request.headers);
    if (!token || !store.sessions[token]) return null;
    return store.users[store.sessions[token].username] || null;
  }

  async currentUserOrBasic(store) {
    const user = await this.currentUser(store);
    if (user) return user;
    const credentials = basicCredentials(this.request.headers);
    if (!credentials) return null;
    const found = store.users[credentials.username];
    if (!found) return null;
    return (await verifyPassword(credentials.password, found.password_hash)) ? found : null;
  }

  async loadStore() {
    const object = await this.bucket.get(STATE_KEY);
    if (object) return normalizeStore(await object.json());
    const admin = await newUser(DEFAULT_ADMIN, DEFAULT_PASSWORD, 'admin', ['/',], true, true, true);
    const store = normalizeStore({
      users: { [DEFAULT_ADMIN]: admin },
      sessions: {},
      public_files: {},
      public_copies: {},
      file_grants: {},
      download_counts: {},
      fresh_setup: true,
    });
    await this.saveStore(store);
    await this.putDirectoryMarker('/public');
    return store;
  }

  async saveStore(store) {
    await this.bucket.put(STATE_KEY, JSON.stringify(store, null, 2), { httpMetadata: { contentType: 'application/json' } });
  }

  async loadAuditLogs() {
    const object = await this.bucket.get(AUDIT_KEY);
    return object ? object.json() : [];
  }

  async recordAudit(store, username, action, path, status, detail) {
    const logs = await this.loadAuditLogs();
    logs.push({ at: nowIso(), username, action, path, status: String(status), detail, ip: clientIp(this.request.headers) });
    const trimmed = logs.slice(-1000);
    await this.bucket.put(AUDIT_KEY, JSON.stringify(trimmed, null, 2), { httpMetadata: { contentType: 'application/json' } });
  }
}

function normalizeStore(store) {
  return {
    users: store.users || {},
    sessions: store.sessions || {},
    public_files: store.public_files || {},
    public_copies: store.public_copies || {},
    file_grants: store.file_grants || {},
    download_counts: store.download_counts || {},
    fresh_setup: store.fresh_setup !== false,
  };
}

function isFunctionRoute(pathname) {
  return pathname.startsWith('/api/') || pathname === '/api' || pathname.startsWith('/public/') || pathname.startsWith('/p/');
}

function fileEntry(store, entry) {
  const publicPath = publicPathForSource(store, entry.path);
  return {
    ...entry,
    download_url: `/api/files${encodePath(entry.path)}`,
    download_count: store.download_counts[entry.path] || 0,
    public: Boolean(publicPath),
    public_url: publicPath ? publicUrlForSourcePath(entry.path) : null,
    granted_users: store.file_grants[entry.path]?.users || [],
  };
}

async function userFromPayload(payload) {
  return newUser(
    String(payload.username || '').trim(),
    String(payload.password || DEFAULT_PASSWORD),
    payload.role === 'admin' ? 'admin' : 'user',
    Array.isArray(payload.roots) ? payload.roots : [],
    Boolean(payload.can_upload),
    Boolean(payload.can_delete),
    Boolean(payload.can_publish),
  );
}

async function newUser(username, password, role, roots, canUpload, canDelete, canPublish) {
  return {
    username,
    password_hash: await hashPassword(password),
    role,
    permissions: effectivePermissions(username, role, roots, canUpload, canDelete, canPublish),
    created_at: nowIso(),
  };
}

function permissionsFromPayload(payload) {
  return effectivePermissions(
    String(payload.username || '').trim(),
    payload.role === 'admin' ? 'admin' : 'user',
    Array.isArray(payload.roots) ? payload.roots : [],
    Boolean(payload.can_upload),
    Boolean(payload.can_delete),
    Boolean(payload.can_publish),
  );
}

function effectivePermissions(username, role, roots, canUpload, canDelete, canPublish) {
  const userRoot = `/user/${username}`;
  const normalizedRoots = roots.map(normalizePath).filter((root) => role === 'admin' || root !== '/');
  const finalRoots = normalizedRoots.length
    ? [...normalizedRoots]
    : (role === 'admin' ? ['/'] : [userRoot, '/public']);
  if (role !== 'admin' && !finalRoots.includes('/public')) finalRoots.push('/public');
  return {
    roots: finalRoots,
    can_upload: canUpload,
    can_delete: role === 'admin' || canDelete,
    can_publish: role === 'admin' || canPublish,
  };
}

function userView(user) {
  return { username: user.username, role: user.role, permissions: user.permissions, created_at: user.created_at };
}

function canRead(user, path) {
  if (user.role === 'admin') return true;
  return (user.permissions.roots || []).map(normalizePath).some((root) => root === '/' || path === root || path.startsWith(`${root.replace(/\/$/, '')}/`));
}

function canAccessPath(store, user, path) {
  return canRead(user, path) || Boolean(store.file_grants[path]?.users?.includes(user.username));
}

function isPublicPath(path) {
  return path === '/public' || path.startsWith('/public/');
}

function publicPathForSource(store, sourcePath) {
  return store.public_files[sourcePath] ? sourcePath : null;
}

function publicSourcePath(store, publicPath) {
  const source = sourcePathFromPublicUrl(publicPath);
  if (store.public_files[source]) return source;
  if (store.public_files[publicPath]) return publicPath;
  return null;
}

function normalizePath(path) {
  let decoded = String(path || '/');
  try { decoded = decodeURIComponent(decoded); } catch {}
  const parts = decoded.split('/').filter((part) => part && part !== '.' && part !== '..');
  return parts.length ? `/${parts.join('/')}` : '/';
}

function decodeRoutePath(path) {
  return normalizePath(path || '/');
}

function joinVirtual(parent, name) {
  const root = normalizePath(parent);
  return root === '/' ? `/${name}` : `${root.replace(/\/$/, '')}/${name}`;
}

function parentPath(path) {
  const parts = normalizePath(path).split('/').filter(Boolean);
  parts.pop();
  return parts.length ? `/${parts.join('/')}` : '/';
}

function basename(path) {
  return normalizePath(path).split('/').filter(Boolean).pop() || '';
}

function encodePath(path) {
  const encoded = normalizePath(path).split('/').filter(Boolean).map(encodeURIComponent).join('/');
  return `/${encoded}`;
}

function normalizePublicRequestPath(path) {
  const normalized = normalizePath(path);
  return isPublicPath(normalized) ? normalized : joinVirtual('/public', normalized.replace(/^\//, ''));
}

function publicUrlForSourcePath(path) {
  return `/public${encodePath(path).replace(/^\/public/, '')}`;
}

function sourcePathFromPublicUrl(path) {
  const normalized = normalizePath(path);
  return isPublicPath(normalized) ? normalizePath(normalized.slice('/public'.length)) : normalized;
}

function fileKey(path) {
  return `files${encodePath(path)}`;
}

function filePrefix(path) {
  return `${fileKey(path).replace(/\/$/, '')}/`;
}

function pathFromFileKey(key) {
  return normalizePath(key.replace(/^files\/?/, ''));
}

function cleanFileName(fileName) {
  const parts = String(fileName || 'upload.bin').split(/[\\/]/).filter(Boolean);
  return parts.pop() || 'upload.bin';
}

function cleanDirectoryName(name) {
  const value = String(name || '').trim();
  if (!value || value === '.' || value === '..' || value.includes('/') || value.includes('\\')) return null;
  return value;
}

async function readJson(request) {
  try { return await request.json(); } catch { return {}; }
}

function json(value, headers = {}, status = 200) {
  return new Response(JSON.stringify(value), { status, headers: { 'content-type': 'application/json; charset=utf-8', ...headers } });
}

function jsonError(status, message) {
  return json({ error: message }, {}, status);
}

function basicAuthRequired() {
  return json({ error: '请先登录' }, {
    'www-authenticate': 'Basic realm="Remote File", charset="UTF-8"',
    'cache-control': 'no-store, no-cache, must-revalidate',
  }, 401);
}

function sessionToken(headers) {
  const cookie = headers.get('cookie') || '';
  for (const part of cookie.split(';')) {
    const [key, ...rest] = part.trim().split('=');
    if (key === 'rf_session') return rest.join('=');
  }
  return null;
}

function basicCredentials(headers) {
  const value = headers.get('authorization') || '';
  if (!value.startsWith('Basic ')) return null;
  try {
    const decoded = atob(value.slice(6));
    const index = decoded.indexOf(':');
    if (index < 0) return null;
    return { username: decoded.slice(0, index), password: decoded.slice(index + 1) };
  } catch {
    return null;
  }
}

function clientIp(headers) {
  const forwarded = headers.get('x-forwarded-for');
  if (forwarded) return forwarded.split(',')[0].trim();
  return headers.get('cf-connecting-ip') || headers.get('x-real-ip') || null;
}

async function hashPassword(password) {
  const salt = randomToken();
  const hex = await sha256Hex(`${salt}:${password}`);
  return `v1$${salt}$${hex}`;
}

async function verifyPassword(password, stored) {
  const parts = String(stored || '').split('$');
  if (parts.length === 3 && parts[0] === 'v1') {
    return (await sha256Hex(`${parts[1]}:${password}`)) === parts[2];
  }
  return (await sha256Hex(password)) === stored;
}

async function sha256Hex(value) {
  const bytes = new TextEncoder().encode(value);
  const hash = await crypto.subtle.digest('SHA-256', bytes);
  return [...new Uint8Array(hash)].map((byte) => byte.toString(16).padStart(2, '0')).join('');
}

function randomToken() {
  const bytes = new Uint8Array(32);
  crypto.getRandomValues(bytes);
  return btoa(String.fromCharCode(...bytes)).replace(/=+$/, '');
}

function nowIso() {
  return new Date().toISOString();
}

function contentTypeForPath(path) {
  const ext = basename(path).toLowerCase().split('.').pop();
  return ({
    html: 'text/html; charset=utf-8', htm: 'text/html; charset=utf-8', txt: 'text/plain; charset=utf-8',
    css: 'text/css; charset=utf-8', js: 'application/javascript; charset=utf-8', json: 'application/json; charset=utf-8',
    png: 'image/png', jpg: 'image/jpeg', jpeg: 'image/jpeg', gif: 'image/gif', webp: 'image/webp', svg: 'image/svg+xml',
    pdf: 'application/pdf', zip: 'application/zip', mp4: 'video/mp4', mp3: 'audio/mpeg', wav: 'audio/wav',
  })[ext] || 'application/octet-stream';
}

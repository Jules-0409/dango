(() => {
  const listeners = new Map();
  const source = new EventSource('/events');

  async function request(path, options = {}) {
    const response = await fetch(path, {
      cache: 'no-store',
      ...options,
      headers: {
        ...(options.body ? { 'Content-Type': 'application/json' } : {}),
        ...options.headers,
      },
    });
    if (!response.ok) {
      const detail = await response.text();
      throw new Error(`${response.status} ${response.statusText}${detail ? `: ${detail}` : ''}`);
    }
    return response.json();
  }

  function emit(type, payload) {
    for (const listener of listeners.get(type) || []) listener(payload);
  }

  for (const type of ['snapshot', 'settings-changed']) {
    source.addEventListener(type, event => {
      try {
        emit(type, JSON.parse(event.data));
      } catch (error) {
        emit('error', new Error(`Invalid ${type} event: ${error}`));
      }
    });
  }
  // EventSource 自己带 3s 重连；断线期间只报一次错，恢复时补一个
  // 'reconnected' 事件让页面主动重拉快照，不用傻等下一轮 SSE。
  let wasDown = false;
  source.onerror = () => {
    if (!wasDown) {
      wasDown = true;
      emit('error', new Error('连接控制 API 的事件流失败'));
    }
  };
  source.onopen = () => {
    if (wasDown) {
      wasDown = false;
      emit('reconnected');
    }
  };

  window.DangoBridge = {
    getSettings: () => request('/settings'),
    saveSettings: settings => request('/settings', {
      method: 'PUT',
      body: JSON.stringify(settings),
    }),
    snapshot: () => request('/snapshot'),
    appMemory: () => request('/app-memory'),
    tokens: (days) => request(`/tokens?days=${encodeURIComponent(days)}`),
    proxyDetail: planId => request(`/proxy-detail/${encodeURIComponent(planId)}`),
    proxyTest: (planId, model) => request(`/proxy-test/${encodeURIComponent(planId)}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ model: model || null }),
    }),
    proxyLogin: planId => request(`/proxy-login/${encodeURIComponent(planId)}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: '{}',
    }),
    proxyLoginStatus: planId => request(`/proxy-login/${encodeURIComponent(planId)}`),
    refresh: () => request('/refresh', { method: 'POST' }),
    connect: plan => request(`/connect/${encodeURIComponent(plan)}`, { method: 'POST' }),
    credentials: () => request('/credentials'),
    setCredential: (plan, token) => request('/credentials', {
      method: 'PUT',
      body: JSON.stringify({ plan, token }),
    }),
    deleteCredential: plan => request(`/credentials/${encodeURIComponent(plan)}`, {
      method: 'DELETE',
    }),
    copyText: async text => {
      try {
        if (!navigator.clipboard?.writeText) throw new Error('此 WebView 不支持剪贴板写入');
        await navigator.clipboard.writeText(text);
      } catch (_) {
        await request('/clipboard', {
          method: 'POST',
          body: JSON.stringify({ text }),
        });
      }
    },
    on: (type, listener) => {
      if (!listeners.has(type)) listeners.set(type, new Set());
      listeners.get(type).add(listener);
      return () => listeners.get(type)?.delete(listener);
    },
  };
})();

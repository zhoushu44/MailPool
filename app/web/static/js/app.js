// MailPool 通用前端工具
(function () {
    function toast(msg, type) {
        const el = document.getElementById('toast');
        if (!el) { console.log('[toast]', msg); return; }
        el.textContent = msg;
        el.className = 'toast show ' + (type || '');
        clearTimeout(el._timer);
        el._timer = setTimeout(() => el.className = 'toast', 3000);
    }
    window.toast = toast;

    // HTML 转义：用于 innerHTML 文本节点
    function escHtml(v) {
        const el = document.createElement('div');
        el.textContent = String(v ?? '');
        return el.innerHTML;
    }
    // HTML 属性转义：escHtml 不处理引号，放进 attr="..." 会被截断
    function escAttr(v) {
        return escHtml(v).replace(/"/g, '&quot;').replace(/'/g, '&#39;');
    }
    window.escHtml = escHtml;
    window.escAttr = escAttr;

    async function api(url, options) {
        const resp = await fetch(url, {
            headers: { 'Content-Type': 'application/json' },
            ...options,
        });
        let data = {};
        try { data = await resp.json(); } catch (e) {}
        if (!resp.ok) {
            var detail = data.detail;
            if (Array.isArray(detail)) {
                // FastAPI 校验错误：拼成可读文本
                detail = detail.map(function (d) {
                    return (d.loc ? d.loc.join('.') + ': ' : '') + (d.msg || '');
                }).join('; ');
            }
            throw new Error(detail || data.msg || data.message || ('HTTP ' + resp.status));
        }
        // 兼容数字 0（/api/admin/*）与字符串 "00000"（对外 /api/v1）两种成功码
        var codeOk = data.code === 0 || data.code === '0' || data.code === '00000';
        if (data.code !== undefined && !codeOk) {
            throw new Error(data.msg || data.message || '业务错误');
        }
        return data;
    }
    window.api = api;

    // data-confirm：必须在捕获阶段拦截，否则元素自身的 onclick 会先执行
    // （实测点「取消」仍会发出删除请求）。
    document.addEventListener('click', function (e) {
        const t = e.target.closest('[data-confirm]');
        if (!t) return;
        if (!window.confirm(t.getAttribute('data-confirm'))) {
            e.preventDefault();
            e.stopImmediatePropagation();
        }
    }, true);

    document.addEventListener('click', function (e) {
        const t = e.target.closest('.mail-toggle');
        if (t) {
            e.preventDefault();
            const body = t.closest('.mail-item').querySelector('.mail-body');
            body.classList.toggle('expanded');
            t.textContent = body.classList.contains('expanded') ? '收起' : '展开';
        }
    });
})();

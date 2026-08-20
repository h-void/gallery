const artistLinksDialogOpeners = new WeakMap();
let artistLinksDialogChromeBound = false;
let artistLinksDialogArtistId = null;
let artistProfileLinksDialogArtistId = null;

function artistLinksDialogElement(dialog) {
  if (typeof dialog === 'string') return document.getElementById(dialog);
  return dialog;
}

function openArtistLinksDialog(dialogId, opener) {
  const dialog = artistLinksDialogElement(dialogId);
  if (!dialog || typeof dialog.showModal !== 'function') return;
  $$('.artist-links-dialog[open]').forEach(other => {
    if (other !== dialog) closeArtistLinksDialog(other);
  });
  if (dialog.open) return;
  artistLinksDialogOpeners.set(dialog, opener && document.contains(opener) ? opener : null);
  dialog.showModal();
}

function closeArtistLinksDialog(dialog) {
  const element = artistLinksDialogElement(dialog);
  if (element && element.open) element.close();
}

function restoreArtistLinksDialogFocus(event) {
  const dialog = event.currentTarget;
  const opener = artistLinksDialogOpeners.get(dialog);
  artistLinksDialogOpeners.delete(dialog);
  if (!opener || !document.contains(opener) || opener.hidden || opener.offsetParent === null) return;
  opener.focus({preventScroll: true});
}

function bindArtistLinksDialog(dialog) {
  if (!dialog || dialog.dataset.artistLinksDialogBound) return;
  dialog.dataset.artistLinksDialogBound = '1';
  dialog.addEventListener('close', restoreArtistLinksDialogFocus);
  dialog.addEventListener('cancel', event => {
    event.preventDefault();
    closeArtistLinksDialog(dialog);
  });
  dialog.addEventListener('click', event => {
    if (event.target === dialog) closeArtistLinksDialog(dialog);
  });
}

function bindArtistLinksDialogChrome() {
  if (artistLinksDialogChromeBound) return;
  artistLinksDialogChromeBound = true;
  $$('[data-artist-links-dialog-close]').forEach(button => {
    button.addEventListener('click', () => {
      closeArtistLinksDialog(button.dataset.artistLinksDialogClose);
    });
  });
}

function resetArtistLinks() {
  state.artistLinks = null;
  state.artistLinksLoading = false;
  state.artistLinksCategory = 'all';
  state.artistLinksProvider = 'all';
  state.artistLinksAvailability = 'all';
  state.artistLinksQuery = '';
  const input = $('#artistLinksSearch');
  if (input) input.value = '';
  closeArtistLinksDialog($('#artistLinksDialog'));
  renderArtistLinks();
}

function isCurrentArtistLinkRequest(artistId, artistSeq) {
  return state.currentArtist
    && Number(state.currentArtist.id) === Number(artistId)
    && isCurrentRequestSeq('artistLoadSeq', artistSeq);
}

async function loadArtistLinks(artistId = state.currentArtist?.id, artistSeq = null) {
  if (!artistId || !state.currentArtist || Number(state.currentArtist.id) !== Number(artistId)) return;
  state.artistLinksLoading = true;
  renderArtistLinks();
  try {
    const links = await API.get(API.artistLinksUrl(artistId));
    if (!state.currentArtist || Number(state.currentArtist.id) !== Number(artistId)) return;
    if (artistSeq != null && !isCurrentRequestSeq('artistLoadSeq', artistSeq)) return;
    state.artistLinks = links && typeof links === 'object' ? links : {links: [], documents: [], summary: {}};
  } catch (error) {
    if (!state.currentArtist || Number(state.currentArtist.id) !== Number(artistId)) return;
    if (artistSeq != null && !isCurrentRequestSeq('artistLoadSeq', artistSeq)) return;
    state.artistLinks = {links: [], documents: [], summary: {}, error: error.message || String(error)};
  } finally {
    const currentArtistMatches = state.currentArtist && Number(state.currentArtist.id) === Number(artistId);
    const currentRequestMatches = artistSeq == null || isCurrentRequestSeq('artistLoadSeq', artistSeq);
    if (currentArtistMatches && currentRequestMatches) {
      state.artistLinksLoading = false;
      renderArtistLinks();
    }
  }
}

function artistLinkCategoryLabel(category) {
  return category === 'cloud_drive' ? '网盘' : '其他链接';
}

function artistLinkProviderKey(link) {
  return link.provider_name || link.host || 'unknown';
}

function artistLinkAvailability(link) {
  return link.availability || 'unverified';
}

function updateArtistLinkFilters(links) {
  const category = $('#artistLinksCategory');
  const provider = $('#artistLinksProvider');
  const availability = $('#artistLinksAvailability');
  if (!category || !provider || !availability) return;
  const categories = [...new Set(links.map(link => link.category || 'other'))].sort();
  const providers = [...new Set(links.map(artistLinkProviderKey))].sort((a, b) => a.localeCompare(b));
  if (state.artistLinksCategory !== 'all' && !categories.includes(state.artistLinksCategory)) {
    state.artistLinksCategory = 'all';
  }
  if (state.artistLinksProvider !== 'all' && !providers.includes(state.artistLinksProvider)) {
    state.artistLinksProvider = 'all';
  }
  const availabilityStates = [...new Set(links.map(artistLinkAvailability))].sort();
  if (state.artistLinksAvailability !== 'all' && !availabilityStates.includes(state.artistLinksAvailability)) {
    state.artistLinksAvailability = 'all';
  }
  category.innerHTML = `<option value="all">全部链接</option>${categories.map(value =>
    `<option value="${escHtml(value)}"${value === state.artistLinksCategory ? ' selected' : ''}>${escHtml(artistLinkCategoryLabel(value))}</option>`
  ).join('')}`;
  provider.innerHTML = `<option value="all">全部服务</option>${providers.map(value =>
    `<option value="${escHtml(value)}"${value === state.artistLinksProvider ? ' selected' : ''}>${escHtml(value)}</option>`
  ).join('')}`;
  availability.innerHTML = `<option value="all">全部状态</option>${availabilityStates.map(value =>
    `<option value="${escHtml(value)}"${value === state.artistLinksAvailability ? ' selected' : ''}>${value === 'unverified' ? '未校验' : escHtml(value)}</option>`
  ).join('')}`;
}

function filteredArtistLinks(links) {
  const query = String(state.artistLinksQuery || '').trim();
  return links.filter(link => {
    if (state.artistLinksCategory !== 'all' && link.category !== state.artistLinksCategory) return false;
    if (state.artistLinksProvider !== 'all' && artistLinkProviderKey(link) !== state.artistLinksProvider) return false;
    if (state.artistLinksAvailability !== 'all' && artistLinkAvailability(link) !== state.artistLinksAvailability) return false;
    if (!query) return true;
    return searchableTextMatches(
      query,
      link.host,
      link.provider_name,
      link.display_url,
      ...(link.sources || []).flatMap(source => [source.file_name, source.folder_name, source.label, source.context])
    );
  });
}

function artistLinkSourceFolder(source) {
  const path = String(source.file_path || '').replace(/\\/g, '/');
  const artistPath = String(state.currentArtist?.path || '').replace(/\\/g, '/').replace(/\/$/, '');
  const parent = path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : '';
  if (!artistPath || parent === artistPath) return '';
  if (parent.startsWith(`${artistPath}/`)) return parent.slice(artistPath.length + 1);
  return '';
}

function artistLinkSourceMarkup(source) {
  const file = source.file_name || '来源文件';
  const path = String(source.file_path || '').replace(/\\/g, '/');
  const parentDir = path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : path;
  const location = source.line_number > 0 ? `第 ${source.line_number} 行` : '';
  const meta = [location, source.label].filter(Boolean).join(UI_FIELD_SEPARATOR);
  const passcode = source.passcode ? `<code>提取码 ${escHtml(source.passcode)}</code>` : '';
  const folder = artistLinkSourceFolder(source);
  const artistId = state.currentArtist ? String(state.currentArtist.id) : '';
  const context = source.context ? `<small class="artist-link-context">${escHtml(source.context)}</small>` : '';
  return `
    <li title="${escHtml(path)}">
      <span class="artist-link-source-file">文件：${escHtml(file)}</span>
      <span class="artist-link-source-path">路径：${escHtml(parentDir || path)}</span>
      ${meta ? `<em>${escHtml(meta)}</em>` : ''}
      ${passcode}
      <button class="btn btn-ghost btn-sm" type="button" data-artist-link-source-folder="${escHtml(folder)}" data-artist-link-source-artist="${escHtml(artistId)}">定位文件夹</button>
      ${context}
    </li>
  `;
}

function artistLinkMarkup(link) {
  const provider = artistLinkProviderKey(link);
  const passcodes = Array.isArray(link.passcodes) ? link.passcodes.filter(Boolean) : [];
  const copyWithCode = passcodes.length
    ? `<button class="btn btn-ghost btn-sm" type="button" data-artist-link-copy-code="${escHtml(`${link.url}\n提取码: ${passcodes[0]}`)}">复制含码</button>`
    : '';
  const sourceCount = Number(link.source_count || (link.sources || []).length || 0);
  const sources = (link.sources || []).map(artistLinkSourceMarkup).join('');
  return `
    <article class="artist-link-row">
      <div class="artist-link-main">
        <span class="artist-link-provider ${link.category === 'cloud_drive' ? 'cloud-drive' : ''}">${escHtml(provider)}</span>
        <a href="${escHtml(link.url)}" target="_blank" rel="noopener noreferrer" title="${escHtml(link.display_url || link.url)}">${escHtml(link.display_url || link.url)}</a>
      </div>
      <div class="artist-link-actions">
        ${passcodes.length ? `<code class="artist-link-passcode">提取码 ${escHtml(passcodes.join(' / '))}</code>` : ''}
        <button class="btn btn-ghost btn-sm" type="button" data-artist-link-copy="${escHtml(link.url)}">复制链接</button>
        ${copyWithCode}
      </div>
      <details class="artist-link-sources">
        <summary>来自 ${sourceCount} 个位置</summary>
        <ul>${sources}</ul>
      </details>
    </article>
  `;
}

function renderArtistLinks() {
  const opener = $('#artistLinksOpenBtn');
  const dialog = $('#artistLinksDialog');
  const summary = $('#artistLinksSummary');
  const status = $('#artistLinksStatus');
  const list = $('#artistLinksList');
  if (!opener || !dialog || !summary || !status || !list) return;
  if (!state.currentArtist || state.mode === 'moves') {
    opener.hidden = true;
    closeArtistLinksDialog(dialog);
    artistLinksDialogArtistId = null;
    return;
  }
  opener.hidden = false;
  const reindex = $('#artistLinksReindexBtn');
  const reindexBusy = isActionBusy('artist-links-reindex', state.currentArtist.id);
  if (reindex) {
    reindex.disabled = reindexBusy;
    reindex.textContent = reindexBusy ? '提取中' : '重新提取链接';
  }
  if (dialog.open && artistLinksDialogArtistId !== Number(state.currentArtist.id)) {
    closeArtistLinksDialog(dialog);
  }
  artistLinksDialogArtistId = Number(state.currentArtist.id);
  const data = state.artistLinks || {links: [], documents: [], summary: {}};
  const links = Array.isArray(data.links) ? data.links : [];
  const documents = Array.isArray(data.documents) ? data.documents : [];
  const counts = data.summary || {};
  updateArtistLinkFilters(links);
  if (state.artistLinksLoading) {
    summary.textContent = '读取中...';
    status.textContent = '正在读取画师说明文件与提取网盘链接...';
    list.innerHTML = '';
    return;
  }
  if (data.error) {
    summary.textContent = '读取失败';
    status.textContent = data.error;
    list.innerHTML = '';
    return;
  }
  summary.textContent = `${counts.cloud_drives || 0} 个网盘 \u00b7 ${counts.links || 0} 条链接`;
  const errors = Number(counts.errors || 0);
  status.textContent = documents.length
    ? `已扫描 ${documents.length} 个文本文件${errors ? `（${errors} 个文件未能解析）` : ''}`
    : '未找到已扫描的 .txt、.html 或 .htm 说明文件';
  const visible = filteredArtistLinks(links);
  if (!visible.length) {
    list.innerHTML = `<div class="artist-links-empty">${links.length ? '没有符合当前筛选条件的链接' : '暂无已索引链接'}</div>`;
    return;
  }
  visible.sort((left, right) => {
    const leftCloud = left.category === 'cloud_drive' ? 0 : 1;
    const rightCloud = right.category === 'cloud_drive' ? 0 : 1;
    return leftCloud - rightCloud || artistLinkProviderKey(left).localeCompare(artistLinkProviderKey(right));
  });
  list.innerHTML = visible.map(artistLinkMarkup).join('');
  $$('[data-artist-link-copy]').forEach(button => {
    button.addEventListener('click', async () => {
      const ok = await copyText(button.dataset.artistLinkCopy || '');
      toast(ok ? '链接已复制' : '复制失败', ok ? 'success' : 'error');
    });
  });
  $$('[data-artist-link-copy-code]').forEach(button => {
    button.addEventListener('click', async () => {
      const ok = await copyText(button.dataset.artistLinkCopyCode || '');
      toast(ok ? '链接和提取码已复制' : '复制失败', ok ? 'success' : 'error');
    });
  });
  $$('[data-artist-link-source-folder]').forEach(button => {
    button.addEventListener('click', () => {
      const artist = state.currentArtist;
      const dialog = $('#artistLinksDialog');
      if (!artist || state.mode === 'moves') return;
      if (button.dataset.artistLinkSourceArtist && Number(button.dataset.artistLinkSourceArtist) !== Number(artist.id)) return;
      const folder = button.dataset.artistLinkSourceFolder || '';
      if (!folderTreeHasPath(state.folders, folder)) {
        toast('未找到目标文件夹，无法定位', 'error');
        return;
      }
      closeArtistLinksDialog(dialog);
      selectFolder(folder);
    });
  });
}

function bindArtistLinks() {
  bindArtistLinksDialogChrome();
  const openBtn = $('#artistLinksOpenBtn');
  const dialog = $('#artistLinksDialog');
  if (openBtn && dialog) {
    openBtn.addEventListener('click', () => openArtistLinksDialog(dialog.id, openBtn));
    bindArtistLinksDialog(dialog);
  }
  const search = $('#artistLinksSearch');
  const category = $('#artistLinksCategory');
  const provider = $('#artistLinksProvider');
  const availability = $('#artistLinksAvailability');
  const reindex = $('#artistLinksReindexBtn');
  if (search) {
    search.addEventListener('input', () => {
      state.artistLinksQuery = search.value;
      renderArtistLinks();
    });
  }
  if (category) {
    category.addEventListener('change', () => {
      state.artistLinksCategory = category.value;
      renderArtistLinks();
    });
  }
  if (provider) {
    provider.addEventListener('change', () => {
      state.artistLinksProvider = provider.value;
      renderArtistLinks();
    });
  }
  if (availability) {
    availability.addEventListener('change', () => {
      state.artistLinksAvailability = availability.value;
      renderArtistLinks();
    });
  }
  if (reindex) {
    reindex.addEventListener('click', async () => {
      const artist = state.currentArtist;
      if (!artist || isActionBusy('artist-links-reindex', artist.id)) return;
      const artistId = artist.id;
      const artistSeq = state.artistLoadSeq;
      setActionBusy('artist-links-reindex', artistId, true);
      renderArtistLinks();
      try {
        const result = await API.post(API.artistLinksReindexUrl(artistId));
        if (!isCurrentArtistLinkRequest(artistId, artistSeq)) return;
        await loadArtistLinks(artistId, artistSeq);
        if (!isCurrentArtistLinkRequest(artistId, artistSeq)) return;
        toast(`链接索引完成：${result.links || 0} 条`, 'success');
      } catch (error) {
        if (isCurrentArtistLinkRequest(artistId, artistSeq)) {
          toast(`链接索引失败：${error.message || error}`, 'error');
        }
      } finally {
        setActionBusy('artist-links-reindex', artistId, false);
        if (isCurrentArtistLinkRequest(artistId, artistSeq)) renderArtistLinks();
      }
    });
  }
}

function resetArtistProfileLinks() {
  state.artistProfileLinks = null;
  state.artistProfileLinksLoading = false;
  const kind = $('#artistProfileLinkKind');
  const platform = $('#artistProfileLinkPlatform');
  const url = $('#artistProfileLinkUrl');
  if (kind) kind.value = 'social';
  if (platform) platform.value = '';
  if (url) url.value = '';
  closeArtistLinksDialog($('#artistProfileLinksDialog'));
  renderArtistProfileLinks();
}

async function loadArtistProfileLinks(artistId = state.currentArtist?.id, artistSeq = null) {
  if (!artistId || !state.currentArtist || Number(state.currentArtist.id) !== Number(artistId)) return;
  state.artistProfileLinksLoading = true;
  renderArtistProfileLinks();
  try {
    const profileLinks = await API.get(API.artistProfileLinksUrl(artistId));
    if (!state.currentArtist || Number(state.currentArtist.id) !== Number(artistId)) return;
    if (artistSeq != null && !isCurrentRequestSeq('artistLoadSeq', artistSeq)) return;
    state.artistProfileLinks = profileLinks && typeof profileLinks === 'object'
      ? profileLinks
      : {links: [], summary: {social: 0, subscription: 0}};
  } catch (error) {
    if (!state.currentArtist || Number(state.currentArtist.id) !== Number(artistId)) return;
    if (artistSeq != null && !isCurrentRequestSeq('artistLoadSeq', artistSeq)) return;
    state.artistProfileLinks = {links: [], summary: {}, error: error.message || String(error)};
  } finally {
    const currentArtistMatches = state.currentArtist && Number(state.currentArtist.id) === Number(artistId);
    const currentRequestMatches = artistSeq == null || isCurrentRequestSeq('artistLoadSeq', artistSeq);
    if (currentArtistMatches && currentRequestMatches) {
      state.artistProfileLinksLoading = false;
      renderArtistProfileLinks();
    }
  }
}

function artistProfileLinkKindLabel(kind) {
  return kind === 'subscription' ? '订阅支持' : '社交媒体';
}

function artistProfileLinkRowMarkup(link) {
  const id = Number(link.id);
  const platform = link.platform || link.host || '未命名平台';
  return `
    <article class="artist-profile-link-row">
      <strong title="${escHtml(platform)}">${escHtml(platform)}</strong>
      <a href="${escHtml(link.url)}" target="_blank" rel="noopener noreferrer" title="${escHtml(link.url)}">${escHtml(link.url)}</a>
      <button class="btn btn-ghost btn-sm" type="button" data-artist-profile-link-delete="${id}">删除</button>
    </article>
  `;
}

function artistProfileLinkGroupMarkup(kind, links) {
  if (!links.length) return '';
  return `
    <section class="artist-profile-link-group">
      <h3>${artistProfileLinkKindLabel(kind)}</h3>
      ${links.map(artistProfileLinkRowMarkup).join('')}
    </section>
  `;
}

function bindArtistProfileLinkDeletes() {
  $$('[data-artist-profile-link-delete]').forEach(button => {
    button.addEventListener('click', async () => {
      const artist = state.currentArtist;
      const linkId = Number(button.dataset.artistProfileLinkDelete);
      if (!artist || !Number.isSafeInteger(linkId) || isActionBusy('artist-profile-link-delete', linkId)) return;
      const artistId = artist.id;
      const artistSeq = state.artistLoadSeq;
      if (!window.confirm('确定删除该链接？')) return;
      if (!isCurrentArtistLinkRequest(artistId, artistSeq)) return;
      setActionBusy('artist-profile-link-delete', linkId, true);
      button.disabled = true;
      try {
        await API.del(API.artistProfileLinkUrl(artistId, linkId));
        if (!isCurrentArtistLinkRequest(artistId, artistSeq)) return;
        await loadArtistProfileLinks(artistId, artistSeq);
        if (!isCurrentArtistLinkRequest(artistId, artistSeq)) return;
        toast('主页链接已删除', 'success');
      } catch (error) {
        if (isCurrentArtistLinkRequest(artistId, artistSeq)) {
          toast(`删除失败：${error.message || error}`, 'error');
        }
      } finally {
        setActionBusy('artist-profile-link-delete', linkId, false);
        if (isCurrentArtistLinkRequest(artistId, artistSeq)) renderArtistProfileLinks();
      }
    });
  });
}

function renderArtistProfileLinks() {
  const opener = $('#artistProfileLinksOpenBtn');
  const dialog = $('#artistProfileLinksDialog');
  const summary = $('#artistProfileLinksSummary');
  const status = $('#artistProfileLinksStatus');
  const list = $('#artistProfileLinksList');
  if (!opener || !dialog || !summary || !status || !list) return;
  if (!state.currentArtist || state.mode === 'moves') {
    opener.hidden = true;
    closeArtistLinksDialog(dialog);
    artistProfileLinksDialogArtistId = null;
    return;
  }
  opener.hidden = false;
  const submit = $('#artistProfileLinksForm button[type="submit"]');
  const addBusy = isActionBusy('artist-profile-link-add', state.currentArtist.id);
  if (submit) {
    submit.disabled = addBusy;
    submit.textContent = addBusy ? '保存中' : '添加主页链接';
  }
  if (dialog.open && artistProfileLinksDialogArtistId !== Number(state.currentArtist.id)) {
    closeArtistLinksDialog(dialog);
  }
  artistProfileLinksDialogArtistId = Number(state.currentArtist.id);
  const data = state.artistProfileLinks || {links: [], summary: {social: 0, subscription: 0}};
  const links = Array.isArray(data.links) ? data.links : [];
  const counts = data.summary || {};
  if (state.artistProfileLinksLoading) {
    summary.textContent = '读取中...';
    status.textContent = '正在读取社交主页与订阅链接...';
    list.innerHTML = '';
    return;
  }
  if (data.error) {
    summary.textContent = '读取失败';
    status.textContent = data.error;
    list.innerHTML = '';
    return;
  }
  summary.textContent = `${counts.social || 0} 个社交主页 \u00b7 ${counts.subscription || 0} 个赞助订阅`;
  status.textContent = links.length ? '' : '尚未添加任何主页或订阅链接';
  const social = links.filter(link => link.kind === 'social');
  const subscription = links.filter(link => link.kind === 'subscription');
  list.innerHTML = social.length || subscription.length
    ? `${artistProfileLinkGroupMarkup('social', social)}${artistProfileLinkGroupMarkup('subscription', subscription)}`
    : '';
  bindArtistProfileLinkDeletes();
}

function bindArtistProfileLinks() {
  bindArtistLinksDialogChrome();
  const openBtn = $('#artistProfileLinksOpenBtn');
  const dialog = $('#artistProfileLinksDialog');
  if (openBtn && dialog) {
    openBtn.addEventListener('click', () => openArtistLinksDialog(dialog.id, openBtn));
    bindArtistLinksDialog(dialog);
  }
  const form = $('#artistProfileLinksForm');
  if (!form) return;
  form.addEventListener('submit', async event => {
    event.preventDefault();
    const artist = state.currentArtist;
    const kind = $('#artistProfileLinkKind')?.value || 'social';
    const platform = $('#artistProfileLinkPlatform');
    const url = $('#artistProfileLinkUrl');
    if (!artist || !url || isActionBusy('artist-profile-link-add', artist.id)) return;
    const artistId = artist.id;
    const artistSeq = state.artistLoadSeq;
    setActionBusy('artist-profile-link-add', artistId, true);
    renderArtistProfileLinks();
    try {
      await API.post(API.artistProfileLinksUrl(artistId), {
        kind,
        platform: platform?.value || '',
        url: url.value,
      });
      if (!isCurrentArtistLinkRequest(artistId, artistSeq)) return;
      if (platform) platform.value = '';
      url.value = '';
      await loadArtistProfileLinks(artistId, artistSeq);
      if (!isCurrentArtistLinkRequest(artistId, artistSeq)) return;
      toast('主页链接已保存', 'success');
    } catch (error) {
      if (isCurrentArtistLinkRequest(artistId, artistSeq)) {
        toast(`保存失败：${error.message || error}`, 'error');
      }
    } finally {
      setActionBusy('artist-profile-link-add', artistId, false);
      if (isCurrentArtistLinkRequest(artistId, artistSeq)) renderArtistProfileLinks();
    }
  });
}
